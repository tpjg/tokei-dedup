//! Hand-rolled JSON serialization of the scan result.
//!
//! We could pull serde into the classifier types but the schema is small and stable —
//! a focused writer keeps that crate dependency-free for downstream library users.

use std::path::Path;
use tokei_dedup_classifier::{Finding, ItemRef};
use tokei_dedup_engine::ScanResult;

pub fn print(result: &ScanResult, scan_root: &Path) {
    let mut s = String::with_capacity(1024 + result.findings.len() * 256);
    s.push('{');
    pair(&mut s, "scan_root", &json_str(&scan_root.display().to_string()));
    s.push(',');
    pair(&mut s, "backend", &json_str(result.backend));
    s.push(',');
    pair_num(&mut s, "files_walked", result.files_walked);
    s.push(',');
    pair_num(&mut s, "entries_indexed", result.entries_indexed);
    s.push(',');
    pair_num(&mut s, "candidate_pairs", result.candidate_pairs);
    s.push(',');
    pair_f32(&mut s, "elapsed_secs", result.elapsed_secs);
    s.push(',');
    s.push_str(r#""findings":["#);
    for (i, f) in result.findings.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        write_finding(&mut s, i + 1, f);
    }
    s.push_str("]}");
    println!("{s}");
}

fn write_finding(out: &mut String, rank: usize, f: &Finding) {
    out.push('{');
    pair_num(out, "rank", rank);
    out.push(',');
    pair_f32(out, "score", f.score);
    out.push(',');
    pair_f32(out, "exact_jaccard", f.exact_jaccard);
    out.push(',');
    pair_f32(out, "estimated_jaccard", f.estimated_jaccard);
    out.push(',');
    pair_num(out, "shared", f.shared as usize);
    out.push(',');
    out.push_str(r#""tags":["#);
    for (i, t) in f.tags.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_str(t.as_str()));
    }
    out.push_str("],");
    out.push_str(r#""a":"#);
    write_endpoint(out, &f.a);
    out.push(',');
    out.push_str(r#""b":"#);
    write_endpoint(out, &f.b);
    out.push('}');
}

fn write_endpoint(out: &mut String, item: &ItemRef) {
    out.push('{');
    pair(out, "path", &json_str(&item.path.display().to_string()));
    out.push(',');
    pair(out, "lang", &json_str(&item.lang));
    out.push(',');
    pair_num(out, "unique_fps", item.unique_fps as usize);
    if let Some(g) = &item.granule {
        out.push(',');
        let name = g.fn_name.as_deref().unwrap_or("");
        pair(out, "fn_name", &json_str(name));
        out.push(',');
        pair_num(out, "line_start", g.line_start as usize);
        out.push(',');
        pair_num(out, "line_end", g.line_end as usize);
    }
    out.push('}');
}

fn pair(out: &mut String, key: &str, value_already_json: &str) {
    out.push_str(&json_str(key));
    out.push(':');
    out.push_str(value_already_json);
}

fn pair_num(out: &mut String, key: &str, value: usize) {
    out.push_str(&json_str(key));
    out.push(':');
    out.push_str(&value.to_string());
}

fn pair_f32(out: &mut String, key: &str, value: f32) {
    out.push_str(&json_str(key));
    out.push(':');
    // Avoid NaN/Inf in JSON; clamp to 0 just in case.
    if value.is_finite() {
        out.push_str(&format!("{value:.6}"));
    } else {
        out.push('0');
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_str_escapes_control_chars() {
        assert_eq!(json_str("ok"), r#""ok""#);
        assert_eq!(json_str("a\"b"), r#""a\"b""#);
        assert_eq!(json_str("a\\b"), r#""a\\b""#);
        assert_eq!(json_str("a\nb"), r#""a\nb""#);
    }
}
