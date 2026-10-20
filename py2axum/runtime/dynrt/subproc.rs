//! `asyncio.create_subprocess_exec()` and its `Process` (asyncio.subprocess, as CPython 3.12-3.15 and uvloop
//! run them): `communicate()`, `wait()`, `returncode`, `pid`, `kill()`, `terminate()`, `send_signal()`. The
//! pipes are read with `communicate()` only: `proc.stdout.read()` is refused at run time (py2axum TypeError).
use std::sync::Arc;

use super::ops;
use super::v::*;

/// `subprocess.PIPE`, `subprocess.STDOUT`, `subprocess.DEVNULL`
const PIPE: i64 = -1;
const STDOUT: i64 = -2;
const DEVNULL: i64 = -3;

/// A child process: its pipes stay with it when a `communicate()` is cancelled (`wait_for`), as asyncio's
/// transport keeps them.
pub struct Proc {
    pid: u32,
    child: tokio::sync::Mutex<tokio::process::Child>,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    stdout: tokio::sync::Mutex<Option<tokio::process::ChildStdout>>,
    stderr: tokio::sync::Mutex<Option<tokio::process::ChildStderr>>,
    piped: [bool; 3],
    returncode: parking_lot::Mutex<Option<i64>>,
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

/// `os.fspath()` of an argument: str, bytes or a path. A NUL byte: asyncio raises `ValueError("embedded null
/// byte")`; uvloop (libuv) passes C strings, so the value ends at the first NUL (`"a\0b"` runs with `"a"`): the
/// same in the binary, a documented risk (docs/advanced/security.md)
fn fspath(v: &V, uvloop: bool) -> R<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt;
    let mut raw: Vec<u8> = match v {
        V::Str(s) => s.as_bytes().to_vec(),
        V::Bytes(b) => b.to_vec(),
        V::Native(n) if matches!(&**n, Native::Path(_)) => {
            let Native::Path(p) = &**n else { unreachable!() };
            p.as_bytes().to_vec()
        }
        o => return Err(Exc::type_error(format!("expected str, bytes or os.PathLike object, not {}", o.type_name()))),
    };
    if let Some(i) = raw.iter().position(|b| *b == 0) {
        if !uvloop {
            return Err(Exc::msg(&VALUE_ERROR, "embedded null byte"));
        }
        raw.truncate(i);
    }
    Ok(std::ffi::OsString::from_vec(raw))
}

fn redirect(v: Option<&V>, name: &str) -> R<(std::process::Stdio, bool)> {
    match v {
        None | Some(V::None) => Ok((std::process::Stdio::inherit(), false)),
        Some(V::Int(PIPE)) => Ok((std::process::Stdio::piped(), true)),
        Some(V::Int(DEVNULL)) => Ok((std::process::Stdio::null(), false)),
        Some(o) => Err(Exc::type_error(format!(
            "py2axum: create_subprocess_exec({name}={}) is not supported (PIPE, DEVNULL{} or None)",
            ops::repr(o)?, if name == "stderr" { ", STDOUT" } else { "" }))),
    }
}

/// The OSError of a failed spawn on `what` (the program, or `cwd`): asyncio names the file (`[Errno 2] No such
/// file or directory: 'prog'`), uvloop (libuv) does not.
fn spawn_error(e: std::io::Error, what: &str, uvloop: bool) -> Exc {
    let exc = super::pathio::os_err(e, what);
    if !uvloop {
        return exc;
    }
    let (errno, text) = {
        let a = exc.0.attrs.lock();
        (a.get("errno").cloned().unwrap_or(V::Int(0)), a.get("strerror").cloned().unwrap_or(V::str("")))
    };
    let msg = match (&errno, &text) {
        (V::Int(n), V::Str(t)) => format!("[Errno {n}] {t}"),
        _ => exc.message(),
    };
    let out = Exc::msg(exc.0.class, msg);
    {
        let mut a = out.0.attrs.lock();
        a.insert("errno".into(), errno);
        a.insert("strerror".into(), text);
        a.insert("filename".into(), V::None);
    }
    out
}

/// `await asyncio.create_subprocess_exec(program, *args, stdin=, stdout=, stderr=, cwd=, env=)`
pub async fn exec(args: &[V], kwargs: &[(String, V)], uvloop: bool) -> R {
    for (k, _) in kwargs {
        match k.as_str() {
            "stdin" | "stdout" | "stderr" | "cwd" | "env" => {}
            "shell" => return Err(Exc::msg(&VALUE_ERROR, "shell must be False")),
            "text" | "universal_newlines" if uvloop => return Err(Exc::msg(&VALUE_ERROR, format!("unexpected kwargs: {k}"))),
            "text" | "universal_newlines" => return Err(Exc::msg(&VALUE_ERROR, format!("{k} must be False"))),
            _ => return Err(Exc::type_error(format!(
                "py2axum: create_subprocess_exec({k}=) is not supported (stdin, stdout, stderr, cwd, env)"))),
        }
    }
    let Some(program) = args.first() else {
        return Err(Exc::type_error("create_subprocess_exec() missing 1 required positional argument: 'program'"));
    };
    let mut argv = Vec::with_capacity(args.len());
    for a in args {
        argv.push(fspath(a, uvloop)?);
    }
    let shown = program.as_str().map(str::to_string).unwrap_or_else(|| argv[0].to_string_lossy().into_owned());
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    let (si, pi) = redirect(kw(kwargs, "stdin"), "stdin")?;
    let (so, po) = redirect(kw(kwargs, "stdout"), "stdout")?;
    let (se, pe) = match kw(kwargs, "stderr") {
        Some(V::Int(STDOUT)) => (None, false),
        v => {
            let (s, p) = redirect(v, "stderr")?;
            (Some(s), p)
        }
    };
    cmd.stdin(si);
    // stderr=STDOUT: the child's stderr is the pipe (or file) of its stdout
    let mut merged = None;
    if se.is_none() && po {
        let (r, w) = std::io::pipe().map_err(|e| super::pathio::os_err(e, &shown))?;
        let w2 = w.try_clone().map_err(|e| super::pathio::os_err(e, &shown))?;
        cmd.stdout(w);
        cmd.stderr(w2);
        merged = Some(r);
    } else {
        let devnull = matches!(kw(kwargs, "stdout"), Some(V::Int(DEVNULL)));
        cmd.stdout(so);
        match se {
            Some(s) => {
                cmd.stderr(s);
            }
            None if devnull => {
                cmd.stderr(std::process::Stdio::null());
            }
            None => {
                // the parent's stdout, inherited as the child's stderr
                use std::os::fd::AsFd;
                let fd = std::io::stdout().as_fd().try_clone_to_owned().map_err(|e| super::pathio::os_err(e, &shown))?;
                cmd.stderr(std::process::Stdio::from(fd));
            }
        }
    }
    if let Some(cwd) = kw(kwargs, "cwd").filter(|v| !matches!(v, V::None)) {
        let dir = fspath(cwd, uvloop)?;
        // CPython's child reports which of chdir and exec failed: the error then names the directory
        if let Err(e) = std::fs::read_dir(&dir) {
            let e = if e.raw_os_error() == Some(libc::EACCES) || std::path::Path::new(&dir).is_dir() {
                e
            } else if std::path::Path::new(&dir).exists() {
                std::io::Error::from_raw_os_error(libc::ENOTDIR)
            } else {
                e
            };
            return Err(spawn_error(e, &dir.to_string_lossy(), uvloop));
        }
        cmd.current_dir(dir);
    }
    if let Some(env) = kw(kwargs, "env").filter(|v| !matches!(v, V::None)) {
        cmd.env_clear();
        let V::Dict(d) = env else {
            return Err(Exc::type_error(format!("py2axum: create_subprocess_exec(env=) takes a dict, not {}", env.type_name())));
        };
        let items: Vec<(V, V)> = d.lock().values().cloned().collect();
        for (k, v) in items {
            let key = fspath(&k, uvloop)?;
            if !uvloop && key.as_encoded_bytes().contains(&b'=') {
                return Err(Exc::msg(&VALUE_ERROR, "illegal environment variable name"));
            }
            if uvloop && !matches!(v, V::Str(_) | V::Bytes(_)) {
                return Err(Exc::type_error("all environment values must be bytes or str"));
            }
            cmd.env(key, fspath(&v, uvloop)?);
        }
    }
    let mut child = cmd.spawn().map_err(|e| spawn_error(e, &shown, uvloop))?;
    drop(cmd); // the parent's copies of the merged pipe's write ends
    let pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    if let Some(r) = merged {
        let fd: std::os::fd::OwnedFd = r.into();
        let std_stdout = std::process::ChildStdout::from(fd);
        stdout = Some(tokio::process::ChildStdout::from_std(std_stdout).map_err(|e| super::pathio::os_err(e, &shown))?);
    }
    let stderr = child.stderr.take();
    Ok(V::native(Native::Proc(Arc::new(Proc {
        pid,
        child: tokio::sync::Mutex::new(child),
        stdin: tokio::sync::Mutex::new(stdin),
        stdout: tokio::sync::Mutex::new(stdout),
        stderr: tokio::sync::Mutex::new(stderr),
        piped: [pi, po, pe],
        returncode: parking_lot::Mutex::new(None),
    }))))
}

fn code_of(st: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(c), _) => c as i64,
        (None, Some(s)) => -(s as i64),
        _ => 0,
    }
}

/// The exit status when the child has exited (asyncio's child watcher sets `returncode` without `wait()`)
fn poll(p: &Proc) -> Option<i64> {
    if let Some(rc) = *p.returncode.lock() {
        return Some(rc);
    }
    let mut g = p.child.try_lock().ok()?;
    let st = g.try_wait().ok()??;
    let rc = code_of(st);
    *p.returncode.lock() = Some(rc);
    Some(rc)
}

async fn wait(p: &Proc) -> R {
    if let Some(rc) = *p.returncode.lock() {
        return Ok(V::Int(rc));
    }
    let st = p.child.lock().await.wait().await.map_err(|e| super::pathio::os_err(e, ""))?;
    let rc = code_of(st);
    *p.returncode.lock() = Some(rc);
    Ok(V::Int(rc))
}

async fn read_all<T: tokio::io::AsyncRead + Unpin>(pipe: &tokio::sync::Mutex<Option<T>>) -> R<Option<Vec<u8>>> {
    use tokio::io::AsyncReadExt;
    let mut g = pipe.lock().await;
    let Some(r) = g.as_mut() else { return Ok(None) };
    let mut out = Vec::new();
    r.read_to_end(&mut out).await.map_err(|e| super::pathio::os_err(e, ""))?;
    Ok(Some(out))
}

/// `await proc.communicate(input=None)`: `input` written then stdin closed (a child that exits without reading it
/// is no error, as in asyncio), stdout and stderr read to their end, then the exit awaited
async fn communicate(p: &Proc, input: Option<&V>) -> R {
    use tokio::io::AsyncWriteExt;
    let data = match input {
        None | Some(V::None) => None,
        Some(V::Bytes(b)) => Some(b.clone()),
        Some(o) => return Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", o.type_name()))),
    };
    let feed = async {
        let mut g = p.stdin.lock().await;
        if let Some(mut w) = g.take() {
            if let Some(d) = data {
                let _ = w.write_all(&d).await; // BrokenPipeError / ConnectionResetError ignored, as asyncio
            }
            let _ = w.shutdown().await;
        }
        Ok::<_, Exc>(())
    };
    let (_, out, err) = tokio::try_join!(feed, read_all(&p.stdout), read_all(&p.stderr))?;
    wait(p).await?;
    let b = |x: Option<Vec<u8>>| x.map(|v| V::Bytes(Arc::from(v))).unwrap_or(V::None);
    Ok(V::tuple(vec![b(out), b(err)]))
}

fn signal(p: &Proc, sig: i32) -> R {
    // asyncio raises ProcessLookupError once the process has exited and been awaited
    if poll(p).is_some() {
        return Err(Exc::new(&PROCESS_LOOKUP_ERROR, vec![]));
    }
    // the pid as tokio still holds it (None once reaped: never a recycled pid); while a `wait()` holds the child,
    // its pid (it is reaped only when that wait returns)
    let pid = match p.child.try_lock() {
        Ok(g) => match g.id() {
            Some(pid) => pid,
            None => return Err(Exc::new(&PROCESS_LOOKUP_ERROR, vec![])),
        },
        Err(_) => p.pid,
    };
    // SAFETY: kill(2) on the child's pid, not reaped (above)
    unsafe { libc::kill(pid as libc::pid_t, sig) };
    Ok(V::None)
}

pub async fn method(p: &Arc<Proc>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "communicate" => {
            if let Some((k, _)) = kwargs.iter().find(|(k, _)| k != "input") {
                return Err(Exc::type_error(format!("Process.communicate() got an unexpected keyword argument '{k}'")));
            }
            communicate(p, args.first().or_else(|| kw(kwargs, "input"))).await
        }
        "wait" => wait(p).await,
        "kill" => signal(p, libc::SIGKILL),
        "terminate" => signal(p, libc::SIGTERM),
        "send_signal" => match args.first() {
            Some(V::Int(s)) => signal(p, *s as i32),
            Some(o) => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
            None => Err(Exc::type_error("Process.send_signal() missing 1 required positional argument: 'signal'")),
        },
        _ => Err(Exc::attr_error(format!("'Process' object has no attribute '{name}'"))),
    }
}

pub fn attr(p: &Arc<Proc>, name: &str) -> R {
    match name {
        "pid" => Ok(V::Int(p.pid as i64)),
        "returncode" => Ok(poll(p).map(V::Int).unwrap_or(V::None)),
        "stdin" | "stdout" | "stderr" => {
            let i = ["stdin", "stdout", "stderr"].iter().position(|x| *x == name).unwrap_or(0);
            if p.piped[i] {
                Err(Exc::type_error(format!("py2axum: Process.{name} is not supported (its stream is read and written \
                    with communicate())")))
            } else {
                Ok(V::None)
            }
        }
        _ => Err(Exc::attr_error(format!("'Process' object has no attribute '{name}'"))),
    }
}

pub fn repr(p: &Proc) -> String {
    format!("<Process {}>", p.pid)
}
