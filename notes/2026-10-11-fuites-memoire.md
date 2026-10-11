# Rapport d'analyse : py2axum et fuites mémoire (11/10/2026)

Ce rapport est destiné aux sessions qui travaillent en parallèle sur le projet. Il porte sur le miroir GitHub
`larrymotalavigne/py2axum`, commit `33606ef` (release 0.6.0 + correctif CI). Je n'ai pas eu accès au dépôt de
développement `gitlab.ggcopr.fr`. Avant de corriger quoi que ce soit, vérifiez que les lignes citées
correspondent encore à votre branche.

Aucun fichier du runtime n'a été modifié. Ce rapport contient seulement des constats et des pistes de
correction.

---

## 1. Le projet en bref

py2axum compile à l'avance (AOT) une application FastAPI + SQLAlchemy 2 + Pydantic v2 en un crate Rust
(axum + tokio + sqlx) qui ne contient plus de Python à l'exécution. La conformité est vérifiée par des tests
différentiels : les mêmes requêtes sont envoyées à FastAPI et au binaire, et les réponses doivent être
identiques à l'octet près.

| Zone | Rôle |
|---|---|
| `py2axum/dyn.py` (~8,8 k lignes) | Compilateur du code projet, de Python vers Rust (fichier `gen.rs` généré) |
| `py2axum/libmap.py` | Correspondance entre les bibliothèques Python et le runtime |
| `py2axum/runtime/dynrt/*.rs` (~57 k lignes) | Runtime copié dans chaque crate généré : `v.rs` (valeur dynamique `V`), `orm.rs` (SQLAlchemy), `pyd.rs` (Pydantic), `web.rs` (FastAPI/Starlette), `rawasgi.rs`, `ws.rs`, `agen.rs`… |
| `fixtures/` | Applications de référence pour la conformité (`dynapp` est la principale) |
| `tests/` | Tests pytest du transpileur, harnais `conformance.py`, scénarios |
| `bench/`, `corpus/` | Benchmarks, corpus de la doc FastAPI (90,6 %) et SQLAlchemy |

Workflow de développement : voir `CONTRIBUTING.md`. Le crate généré doit compiler avec
`RUSTFLAGS="-D warnings"`. Il ne faut jamais modifier le code généré : on corrige le transpileur ou le runtime.
Chaque nouveau comportement doit être accompagné d'un cas de conformité dans `fixtures/` et d'un scénario dans
`tests/scenarios/`.

### Point clé pour les fuites : le modèle mémoire

`V` (dans `v.rs`) reproduit `PyObject*` avec des `Arc<Mutex<…>>` : `List`, `Dict`, `Set`, `Inst`, `Obj`,
les fonctions (`PyFn`/`FnVal`) et les cellules de closure (`ops::cell_new()` est un `V::List`).
**`Arc` ne libère jamais les cycles, alors que CPython a un ramasse-miettes cyclique (`gc`).** Tout code
Python qui crée une référence circulaire fuit donc dans le binaire, une fois par exécution. C'est la
première cause de fuite trouvée.

Il existe déjà une protection : `tests/test_runtime_leaks.py` interdit `Box::leak` et `mem::forget` en dehors
des caches de `types.rs`. Ce test ne détecte pas les cycles d'`Arc`, ni les tâches tokio qui ne se terminent
jamais. Le changelog 0.3.1, ce test et `.gitignore` citent un `bench/soak.py` (test d'endurance), **absent du miroir GitHub**. Il existe peut-être sur GitLab : à vérifier avant de le réécrire.

---

## 2. Fuite confirmée par mesure : fonction imbriquée auto-référente

**Gravité : haute.** Ce motif est courant : helper récursif, décorateur qui se renvoie lui-même, callback
qui se ré-enregistre.

Code généré (`dyn.py:3743-3798`, `nested_def`) pour :

```python
@app.get("/selfref_big")
async def selfref_big():
    payload = "x" * 100000
    def walk(n: int) -> int:
        return len(payload) if n <= 0 else walk(n - 1)
    return {"v": walk(3)}
```

```rust
let v_payload: V = ops::cell_new();
let v_walk: V = ops::cell_new();          // cellule = Arc<Mutex<Vec<V>>>
*ops::cell_mut(&v_walk) = pyfn(..., Arc::new({ let c_walk = v_walk.clone(); /* closure */ }));
//  la cellule v_walk contient la fonction → la closure contient la cellule v_walk  ⇒ cycle
```

Il existe deux variantes, et les deux fuient :

- la cellule de variable (cas ci-dessus) ;
- le cas `selfref` : `Arc<OnceLock<V>>` rempli par `cell.set(fonction)` après la définition
  (`dyn.py:3769-3798`).

**Tout ce que la closure capture fuit avec elle**, y compris les autres cellules, les données de la
requête, etc.

### Mesure

Application de reproduction : 5 000 requêtes après 200 requêtes de chauffe, binaire en release, RSS lu dans
`/proc/PID/status`.

| Endpoint | Δ RSS / 5 000 req | Par requête |
|---|---:|---:|
| `/plain` (fonction imbriquée non récursive) | 16 kB | ~0 |
| `/selfref` (factorielle récursive, petite capture) | 4 MB | ~830 B |
| `/selfref_big` (récursive, capture de 100 kB) | 493 MB | **~100 kB** |
| `/cycle_list` (`a.append(a)`) | 489 MB | **~100 kB** |

### Pistes de correction (au choix)

1. **Rompre le cycle par un `Weak`** : la closure capture un `Weak` vers la cellule ou le `OnceLock`, et
   l'appelant garde la référence forte. Attention au cas où la fonction est renvoyée hors de sa portée
   (`return handler`) : le `Weak` serait alors mort. Il faut donc une référence forte détenue par la valeur
   renvoyée, par exemple `PyFn` qui possède sa propre cellule.
2. **Casser le cycle à la sortie de la portée** : à la fin de la fonction englobante, vider (`V::Unbound`)
   les cellules capturées par une fonction auto-référente, si elles ne s'échappent pas. Une analyse
   d'échappement est nécessaire dans `dyn.py`.
3. **Refuser à la compilation** (règle « refuse rather than guess ») le motif quand la fonction s'échappe,
   avec un code d'erreur `P2A…`, et appliquer 1 ou 2 au cas local.

`a.append(a)` et les cycles explicites de dicts ou de listes sont rares en pratique. Il suffit de les
documenter dans `docs/supported.md` comme différence connue (pas de GC cyclique).

---

## 3. Fuites ORM (`orm.rs`) : confirmées à la lecture du code, non mesurées

Ce qui est sain dans la conception :

- `ObjCell.sess` est un `Weak` (`orm.rs:348`) ;
- l'identity map contient des `Weak` (`orm.rs:3158`) ;
- `SessInner.cx` est un `Weak<CxInner>` ;
- `impl Drop for SessInner` (~3181) appelle `unload_rels()` et `links.clear()` sur les objets de
  `identity`, `new` et `deleted`, ce qui casse les cycles parent ↔ enfant.

Toutes les fuites ci-dessous concernent des objets qui **sortent de ces trois collections sans être
déchargés**.

| # | Gravité | Où | Problème | Correctif proposé |
|---|---|---|---|---|
| A | **Haute** | `orm.rs:3632` (`flush_inner`) | `std::mem::take(&mut s.new)` vide la liste, puis l'INSERT échoue (IntegrityError, doublon → 409). L'objet en échec et tous ceux qui le suivent ne sont plus ni dans `new` ni dans `identity`. `sync_rels` a déjà relié parent et enfants (`rels`, `rel_snap`, `links`), donc le cycle fuit **à chaque requête en échec**. Exemple : `db.add(Owner(name=dup, books=[Book()])); await db.commit()`. | Ne retirer de `new` qu'après succès, ou remettre la fin non traitée dans `new` en cas d'erreur. Appeler `unload_rels()` et `links.clear()` sur tout objet abandonné. |
| B | Moyenne | `orm.rs:~4036-4061` (`rollback`) | Les objets `new` passent en `Transient`, ainsi que les objets du `continue` et les `gone`, mais avec `rels` et `links` intacts, avant la boucle `unload_rels`. | Décharger chaque objet oublié. |
| C | Moyenne | `orm.rs:~3818-3829` | Après un DELETE réussi (cascade), les objets supprimés sortent de `deleted` et de `identity` avec des relations chargées dans les deux sens. Même problème avec `SpFrame.deleted` et `SpFrame.inserted`. | `unload_rels()` et `links.clear()` après le DELETE. |
| D | Basse | `rel_get`/`rel_set` (~395, 466-480) | Deux objets transients reliés par back_populates et jamais ajoutés à une session forment un cycle. | Rare. À documenter, ou à traiter avec la solution générale. |
| E | Moy-haute (pool) | `orm.rs:3989` (`Session::close`) | `close()` ne rend pas la connexion : la transaction reste ouverte (« idle in transaction ») jusqu'au drop du `Cx`. Avec des BackgroundTasks longues ou un streaming, le pool de 32 connexions peut s'épuiser. | Dans `close()`, faire `tx.take()` puis un rollback, et expunge (`unload_rels`) comme SQLAlchemy. |

Hors fuite (performance) : `anon_{N}` (`orm.rs:~1655`) met un compteur global dans le SQL de chaque
`.subquery()` anonyme. Le SQL change donc à chaque requête, ce qui provoque un *prepare* à chaque fois et
renouvelle sans cesse le cache de statements sqlx. La taille reste bornée (100 statements par connexion).
Numéroter par rendu dans `Rend`, comme pour les alias, règlerait le problème.

Faible : `SESSION_PARAMS` (`orm.rs:2986`) est étendu à chaque `create_engine(connect_args=)`. Ce n'est
problématique que si un engine est créé par requête. Correctif : remplacer ou dédupliquer par nom plutôt
que faire `extend`.

---

## 4. Cycle de vie des requêtes et des tâches : confirmé à la lecture, non mesuré

| # | Gravité | Où | Problème | Correctif proposé |
|---|---|---|---|---|
| 1 | **Haute** | `web.rs:1100-1123` et `dep_gen` (~1161) | Une réponse en streaming (StreamingResponse, SSE, générateur, `stream_list`) avec une dépendance `yield` (`get_db`) délègue les teardowns à une `stream::once(...)` chaînée après le corps (`guard.0 = None`). **Si le client se déconnecte en cours de flux, cette queue ne s'exécute jamais.** La tâche de la dépendance détient le `Cx` et attend `resume`, dont l'émetteur est dans `cx.teardowns`. Cela forme un cycle tâche ↔ Cx : la tâche, le `CxInner`, la session et **la connexion du pool fuient**. | Un garde `Drop` dans l'état du flux, comme `TeardownGuard`, qui vide `cx.teardowns` (et envoie GeneratorExit) si la queue n'a pas tourné. |
| 2 | Haute/moy | `rawasgi.rs:85-97` (`App::drop` → `spawn`), `receive` 167-184, `forward` 230-244 | Un middleware ASGI brut abandonné (client parti avant `http.response.start`, ou erreur) est relancé dans sa propre tâche sans `chan.complete()`. Un `receive()` suivant attend `done_notify` pour toujours. `forward` peut aussi s'auto-bloquer : `tx_end` se trouve dans un message de `chan.msgs` que la tâche possède elle-même. | `complete()` dans `App::drop` avant le spawn. `select!` entre `rx_end` et `done_notify`, et vider `msgs` dans `complete()`. |
| 3 | Moyenne | `rawasgi.rs:62-65`, 187-225 | La file `VecDeque` des messages ASGI n'a pas de borne. `send` n'échoue jamais après `done` : une appli ASGI brute de streaming (par exemple un ticker SSE) continue d'empiler après le départ du client. | Refuser (ClientDisconnected / OSError) une fois `done` atteint, et borner la file. |
| 4 | Moyenne | `ws.rs:154, 216` | `mpsc::unbounded_channel()` en sortie WebSocket : un client lent face à un `send_text` rapide fait grossir la file sans limite. | Canal borné, `send` qui attend la capacité. |
| 5 | Moy-basse | `web.rs:1710-1784` (`Task { cx: Cx }`), `agen.rs:65-82` | `asyncio.create_task` détient le `Cx` en fort. Si la tâche (ou un générateur) est stockée dans `request.state`, un ContextVar, `cx.deps` ou `scope["state"]`, on obtient un cycle permanent, avec la session et la connexion. | `Weak<CxInner>` dans `Task`, ou vider `deps`, `state`, `asgi_scope` et `background` en fin de requête. |

Problèmes de moindre importance (soupçonnés ou bornés) :

- **Générateurs streamés** (`web.rs:2006-2063`, `agen.rs:173-191`) : la déconnexion n'est détectée qu'au
  `yield` suivant. Un générateur qui attend un pub/sub garde son `cx` jusqu'au prochain élément.
- **`asyncio.gather`** (`aio.rs:129`) : les enfants ne sont pas annulés quand la requête est abandonnée.
  La fuite est temporaire.
- **`STACK.get_or_try_init`** (`asgi.rs:189`) : réexécuté à chaque requête après un échec, et `add_route`
  ajoute dans `ADDED` sans dédoublonner.
- **`types::intern`** (`types.rs:119`) : une chaîne fuit par valeur distincte (`json.dumps(separators=)`,
  `col.op(...)`). Ce n'est grave que si la valeur vient de la requête.
- **`http.rs:217-218`** : chaque `AsyncClient()` construit deux clients reqwest, dont l'« insecure ». C'est
  un coût CPU, pas une fuite.

Vérifiés et sains : `ROOT`, `Handling`, `Slot` (`Weak<Stack>`), le cache regex de `routing.rs` (clés
statiques), ContextVar (par requête), `resp.rs` (background `take`), les breadcrumbs Sentry (plafonnés),
`prom.rs` (cardinalité comme en Python), `mcp.rs` (sans état), les caches `DEFAULTS`/`ORIGINALS`/`FROM_ATTRS`
de `ops.rs`/`pyd.rs` (`Weak` + purge), `PROPS` de `pyd.rs` (nettoyé par le `Drop` de `Pf`), et la
déconnexion client sur une requête non streamée (le `Cx` est abandonné, sqlx fait un ROLLBACK).

---

## 5. Ordre de correction recommandé

1. **Fonction imbriquée auto-référente** (§2) : mesurée, ~100 kB par requête possible, motif fréquent.
2. **Streaming + dépendance `yield` + déconnexion** (§4 #1) et **raw ASGI non complété** (§4 #2) : ils fuient
   la connexion du pool et peuvent bloquer le service. Un seul garde « requête terminée / corps abandonné »
   qui possède les teardowns et `chan.complete()` corrige les deux.
3. **`flush_inner` en échec** (§3 A), puis **`Session::close`** (§3 E).
4. `rollback` et delete (§3 B, C), files non bornées (§4 #3, #4), `Task` ↔ `Cx` (§4 #5).

## 6. Proposition pour empêcher les régressions

- **Étendre (ou recréer) `bench/soak.py`** (absent du miroir GitHub) : lancer le binaire, envoyer N requêtes
  par endpoint, relever `VmRSS` et échouer au-delà d'un seuil par requête. Ajouter un endpoint par motif
  ci-dessus à `fixtures/dynapp` : récursion imbriquée, commit en doublon, streaming avec `get_db` et client
  coupé, middleware ASGI brut avec client coupé.
- **Compteur de vivants** en build de test : un `AtomicUsize` incrémenté et décrémenté dans `CxInner`,
  `SessInner` et `ObjCell` (`Drop`), exposé via un endpoint de debug. C'est plus fiable que le RSS (qui
  dépend de l'allocateur) pour détecter un `Cx` ou une session qui survit.
- Pour le pool : vérifier `pg_stat_activity` (`state = 'idle in transaction'`) après le soak.

## 7. Reproduire la mesure du §2

```bash
# Postgres local (le binaire ouvre le pool au démarrage)
pg_ctlcluster 16 main start
su postgres -c "psql -c \"ALTER USER postgres PASSWORD 'postgres'\" -c 'CREATE DATABASE leak'"

# leakapp/main.py : les 4 endpoints du tableau (§2), FastAPI pur, sans base de données
python -m py2axum /chemin/leakapp --root /chemin -o build --name leakapp   # chemin absolu du package
(cd build && cargo build --release)                                        # ~6 min à froid
DATABASE_URL=postgresql://postgres:postgres@127.0.0.1/leak PORT=8391 ./build/target/release/leakapp &
# 5000 requêtes keep-alive par endpoint, puis comparer VmRSS dans /proc/$PID/status
```

Remarque : `py2axum leakapp` avec un nom relatif résolu depuis un autre répertoire n'a généré que les routes
OpenAPI, sans erreur. Il faut passer le chemin du package : `py2axum /chemin/leakapp`. Ce comportement
silencieux mérite peut-être un message d'erreur.
