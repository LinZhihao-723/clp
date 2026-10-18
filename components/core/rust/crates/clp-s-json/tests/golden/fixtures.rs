//! Format compatibility: every C++ fixture archive renders to the bytes C++ `clp-s x` printed when
//! emulating C++ plain floats, and to canonically equal records (plain floats differing only by
//! DV-01) without the emulation.

use clp_s_table::DEFAULT_BATCH_NUM_ROWS;
use clp_s_table::ResolvedProjection;

use crate::support::EMULATED;
use crate::support::OpenedArchive;
use crate::support::SHORTEST;
use crate::support::missing_from;
use crate::support::oracle_lines;
use crate::support::readable_archives;
use crate::support::sample;
use crate::support::stem;

#[test]
fn every_fixture_renders_the_cpp_bytes_when_emulating_cpp_floats() -> anyhow::Result<()> {
    let mut failures = Vec::new();
    let mut num_records_checked = 0;
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        let expected = oracle_lines(&path)?;
        // Small windows make every delta-encoded column and template binding cross windows.
        for batch_num_rows in [1, 7, DEFAULT_BATCH_NUM_ROWS] {
            let rendered = archive.render(&ResolvedProjection::Full, EMULATED, batch_num_rows)?;
            let unexpected = missing_from(&rendered, &expected);
            let missing = missing_from(&expected, &rendered);
            if rendered.len() != expected.len() || !unexpected.is_empty() || !missing.is_empty() {
                failures.push(format!(
                    "{} ({batch_num_rows}-row windows): {} records rendered, {} expected\n  \
                     unexpected: {:?}\n  missing: {:?}",
                    stem(&path),
                    rendered.len(),
                    expected.len(),
                    sample(&unexpected),
                    sample(&missing),
                ));
            }
            num_records_checked += rendered.len();
        }
    }
    assert!(
        failures.is_empty(),
        "{} fixture renders differ from C++:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        num_records_checked > 3 * 10_000,
        "{num_records_checked} records"
    );
    Ok(())
}

#[test]
fn without_emulation_only_plain_floats_differ_from_cpp() -> anyhow::Result<()> {
    let mut failures = Vec::new();
    let mut num_differing_floats = 0;
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        let emulated =
            archive.render(&ResolvedProjection::Full, EMULATED, DEFAULT_BATCH_NUM_ROWS)?;
        let shortest =
            archive.render(&ResolvedProjection::Full, SHORTEST, DEFAULT_BATCH_NUM_ROWS)?;
        anyhow::ensure!(emulated.len() == shortest.len(), "{}", stem(&path));
        for (cpp, rust) in emulated.iter().zip(&shortest) {
            if let Err(reason) = serde_json::from_slice::<serde_json::Value>(rust) {
                failures.push(format!("{}: invalid JSON ({reason})", stem(&path)));
                continue;
            }
            match compare_floats(cpp, rust) {
                Some(num_floats) => num_differing_floats += num_floats,
                None => failures.push(format!(
                    "{}:\n  C++:  {}\n  Rust: {}",
                    stem(&path),
                    String::from_utf8_lossy(cpp),
                    String::from_utf8_lossy(rust)
                )),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} records differ beyond DV-01:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        num_differing_floats > 10,
        "only {num_differing_floats} plain floats differ: the fixtures do not cover DV-01"
    );
    Ok(())
}

#[test]
fn projecting_every_schema_node_renders_like_the_full_template() -> anyhow::Result<()> {
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        let index = archive.storage.index();
        let mut nodes: Vec<_> = index
            .schema_map()
            .ids()
            .iter()
            .filter_map(|id| index.schema_map().schema(*id))
            .flat_map(|schema| schema.nodes.iter().copied())
            .collect();
        nodes.sort_unstable();
        nodes.dedup();
        let every_node = ResolvedProjection::Nodes(nodes.into_boxed_slice());
        assert_eq!(
            archive.render(&every_node, EMULATED, DEFAULT_BATCH_NUM_ROWS)?,
            archive.render(&ResolvedProjection::Full, EMULATED, DEFAULT_BATCH_NUM_ROWS)?,
            "{}",
            stem(&path)
        );
    }
    Ok(())
}

/// Compares a record rendered with C++ plain floats (`cpp`) with the same record rendered with
/// shortest round-trip floats (`rust`), token by token.
///
/// # Returns
///
/// The number of number tokens that differ, if every difference is a Rust float token that C++
/// `%f` would print as the C++ token (DV-01); `None` otherwise.
fn compare_floats(cpp: &[u8], rust: &[u8]) -> Option<usize> {
    let cpp_tokens = tokens(cpp)?;
    let rust_tokens = tokens(rust)?;
    if cpp_tokens.len() != rust_tokens.len() {
        return None;
    }
    let mut num_differing = 0;
    for (cpp_token, rust_token) in cpp_tokens.iter().zip(&rust_tokens) {
        if cpp_token == rust_token {
            continue;
        }
        let rust_text = std::str::from_utf8(rust_token).ok()?;
        if !rust_text.contains(['.', 'e']) {
            return None;
        }
        let value: f64 = rust_text.parse().ok()?;
        if format!("{value:.6}").as_bytes() != *cpp_token {
            return None;
        }
        num_differing += 1;
    }
    Some(num_differing)
}

/// Splits JSON text into tokens (strings with their quotes, numbers, literals and punctuation),
/// skipping whitespace.
///
/// # Returns
///
/// The tokens, or `None` if a string is unterminated.
fn tokens(text: &[u8]) -> Option<Vec<&[u8]>> {
    let mut tokens = Vec::new();
    let mut position = 0;
    while position < text.len() {
        let begin = position;
        match text[position] {
            b' ' | b'\t' | b'\n' | b'\r' => {
                position += 1;
                continue;
            }
            b'"' => {
                position += 1;
                loop {
                    match *text.get(position)? {
                        b'\\' => position += 2,
                        b'"' => break,
                        _ => position += 1,
                    }
                }
                position += 1;
            }
            byte if byte.is_ascii_alphanumeric() || b'-' == byte => {
                while text
                    .get(position)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(byte))
                {
                    position += 1;
                }
            }
            _ => position += 1,
        }
        tokens.push(&text[begin..position]);
    }
    Some(tokens)
}
