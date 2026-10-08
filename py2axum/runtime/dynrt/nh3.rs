//! `nh3` (Python bindings of the ammonia crate): the same ammonia, configured as nh3 does it
//! (`clean`, `clean_text`/`escape`, `is_html`). Callable options are refused.
use std::collections::{HashMap, HashSet};

use super::ops;
use super::v::*;

fn strs(v: &V) -> R<HashSet<String>> {
    ops::iter(v)?.iter().map(ops::str_).collect()
}

fn str_map(v: &V) -> R<HashMap<String, V>> {
    match v {
        V::Dict(d) => d.lock().values().map(|(k, x)| Ok((ops::str_(k)?, x.clone()))).collect(),
        o => Err(Exc::type_error(format!("argument 'attributes': '{}' object cannot be converted to 'PyDict'", o.type_name()))),
    }
}

const OPTS: [&str; 14] = [
    "tags", "clean_content_tags", "attributes", "attribute_filter", "strip_comments", "link_rel",
    "generic_attribute_prefixes", "tag_attribute_values", "set_tag_attribute_values", "url_schemes",
    "allowed_classes", "filter_style_properties", "url_relative", "id_prefix",
];

/// `nh3.clean(html, **options)`
pub fn clean(args: &[V], kwargs: &[(String, V)]) -> R {
    let mut opt: HashMap<&str, V> = HashMap::new();
    let html = match args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "html").map(|(_, v)| v)) {
        Some(V::Str(s)) => s.to_string(),
        Some(o) => return Err(Exc::type_error(format!("argument 'html': '{}' object cannot be converted to 'PyString'", o.type_name()))),
        None => return Err(Exc::type_error("clean() missing required positional argument: html")),
    };
    for (i, a) in args.iter().enumerate().skip(1) {
        let name = *OPTS.get(i - 1).ok_or_else(|| Exc::type_error("clean() takes at most 15 positional arguments"))?;
        opt.insert(name, a.clone());
    }
    for (k, v) in kwargs {
        if k == "html" {
            continue;
        }
        let name = *OPTS.iter().find(|o| **o == k.as_str()).ok_or_else(|| Exc::type_error(format!("clean() got an unexpected keyword argument '{k}'")))?;
        opt.insert(name, v.clone());
    }
    opt.retain(|_, v| !v.is_none());
    for k in ["attribute_filter", "url_relative", "id_prefix"] {
        if opt.contains_key(k) {
            return Err(Exc::type_error(format!("py2axum: nh3.clean({k}=) is not supported")));
        }
    }
    let tags = opt.get("tags").map(strs).transpose()?;
    let ccontent = opt.get("clean_content_tags").map(strs).transpose()?;
    let attributes: Option<HashMap<String, HashSet<String>>> =
        opt.get("attributes").map(|v| str_map(v)?.into_iter().map(|(k, x)| Ok((k, strs(&x)?))).collect::<R<_>>()).transpose()?;
    let strip_comments = match opt.get("strip_comments") {
        Some(v) => ops::truthy(v)?,
        None => true,
    };
    // link_rel=None is an explicit None (kept above only when not None): look at the raw keyword
    let explicit_none = kwargs.iter().any(|(k, v)| k == "link_rel" && v.is_none()) || matches!(args.get(6), Some(V::None));
    let link_rel = if explicit_none {
        None
    } else {
        Some(opt.get("link_rel").map(ops::str_).transpose()?.unwrap_or_else(|| "noopener noreferrer".into()))
    };
    let prefixes = opt.get("generic_attribute_prefixes").map(strs).transpose()?;
    let nested = |v: &V| -> R<HashMap<String, HashMap<String, V>>> { str_map(v)?.into_iter().map(|(k, x)| Ok((k, str_map(&x)?))).collect() };
    let tav: Option<HashMap<String, HashMap<String, HashSet<String>>>> = opt
        .get("tag_attribute_values")
        .map(|v| nested(v)?.into_iter().map(|(t, m)| Ok((t, m.into_iter().map(|(a, x)| Ok((a, strs(&x)?))).collect::<R<_>>()?))).collect::<R<_>>())
        .transpose()?;
    let stav: Option<HashMap<String, HashMap<String, String>>> = opt
        .get("set_tag_attribute_values")
        .map(|v| nested(v)?.into_iter().map(|(t, m)| Ok((t, m.into_iter().map(|(a, x)| Ok((a, ops::str_(&x)?))).collect::<R<_>>()?))).collect::<R<_>>())
        .transpose()?;
    let schemes = opt.get("url_schemes").map(strs).transpose()?;
    let classes: Option<HashMap<String, HashSet<String>>> =
        opt.get("allowed_classes").map(|v| str_map(v)?.into_iter().map(|(k, x)| Ok((k, strs(&x)?))).collect::<R<_>>()).transpose()?;
    let styles = opt.get("filter_style_properties").map(strs).transpose()?;

    // nh3's own checks (ValueError rather than an ammonia panic)
    if link_rel.is_some() {
        if let Some(attrs) = &attributes {
            if let Some((tag, _)) = attrs.iter().find(|(_, s)| s.contains("rel")) {
                return Err(Exc::value_error(format!(
                    "\"rel\" attribute is not allowed for tag \"{tag}\" when link_rel is set; pass link_rel=None to manage the \"rel\" attribute directly")));
            }
        }
    }
    if let Some(cc) = &ccontent {
        let default_tags = ammonia::Builder::default().clone_tags();
        let conflict = match &tags {
            Some(allowed) => cc.iter().find(|t| allowed.contains(*t)).cloned(),
            None => cc.iter().find(|t| default_tags.contains(t.as_str())).cloned(),
        };
        if let Some(tag) = conflict {
            return Err(Exc::value_error(format!(
                "tag \"{tag}\" cannot appear in both `tags` and `clean_content_tags`; either remove it from `clean_content_tags` or pass an explicit `tags` set that excludes it")));
        }
    }
    if let (Some(values), Some(attrs)) = (&tav, &attributes) {
        let generic = attrs.get("*");
        for (tag, tv) in values {
            for attr in tv.keys() {
                if attrs.get(tag).is_some_and(|s| s.contains(attr)) || generic.is_some_and(|s| s.contains(attr)) {
                    return Err(Exc::value_error(format!(
                        "attribute \"{attr}\" on tag \"{tag}\" is whitelisted in both `attributes` and `tag_attribute_values`, which are alternates; `attributes` already permits every value, so the `tag_attribute_values` whitelist would be silently ignored. Drop \"{attr}\" from `attributes` (the \"{tag}\" entry or \"*\") to restrict it by value")));
                }
            }
        }
    }

    let mut b = ammonia::Builder::default();
    if let Some(t) = &tags {
        b.tags(t.iter().map(String::as_str).collect());
    }
    if let Some(t) = &ccontent {
        b.clean_content_tags(t.iter().map(String::as_str).collect());
    }
    if let Some(a) = &attributes {
        b.tag_attributes(a.iter().filter(|(k, _)| k.as_str() != "*").map(|(k, v)| (k.as_str(), v.iter().map(String::as_str).collect())).collect());
        if let Some(g) = a.get("*") {
            b.generic_attributes(g.iter().map(String::as_str).collect());
        }
    }
    if let Some(p) = &prefixes {
        b.generic_attribute_prefixes(p.iter().map(String::as_str).collect());
    }
    if let Some(v) = &tav {
        b.tag_attribute_values(
            v.iter().map(|(t, m)| (t.as_str(), m.iter().map(|(a, s)| (a.as_str(), s.iter().map(String::as_str).collect())).collect())).collect(),
        );
    }
    if let Some(v) = &stav {
        b.set_tag_attribute_values(v.iter().map(|(t, m)| (t.as_str(), m.iter().map(|(a, s)| (a.as_str(), s.as_str())).collect())).collect());
    }
    b.strip_comments(strip_comments);
    b.link_rel(link_rel.as_deref());
    if let Some(s) = &schemes {
        b.url_schemes(s.iter().map(String::as_str).collect());
    }
    if let Some(c) = &classes {
        b.allowed_classes(c.iter().map(|(t, s)| (t.as_str(), s.iter().map(String::as_str).collect())).collect());
    }
    if let Some(s) = &styles {
        b.filter_style_properties(s.iter().map(String::as_str).collect());
    }
    Ok(V::str(b.clean(&html).to_string()))
}

/// `nh3.clean_text(html)` / `nh3.escape(html)` (without `tags`)
pub fn clean_text(args: &[V], kwargs: &[(String, V)]) -> R {
    if kwargs.iter().any(|(k, v)| k == "tags" && !v.is_none()) || args.len() > 1 {
        return Err(Exc::type_error("py2axum: nh3.clean_text(tags=) is not supported"));
    }
    let html = ops::str_(args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "html").map(|(_, v)| v)).ok_or_else(|| Exc::type_error("clean_text() missing required positional argument: html"))?)?;
    Ok(V::str(ammonia::clean_text(&html)))
}

/// `nh3.is_html(html)`
pub fn is_html(args: &[V]) -> R {
    Ok(V::Bool(ammonia::is_html(&ops::str_(args.first().ok_or_else(|| Exc::type_error("is_html() missing required positional argument: html"))?)?)))
}
