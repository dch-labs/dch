#![cfg(test)]

//! Fixtures shared by the config module's tests.
//!
//! The file writer the load tests build a config directory with.

use std::io::Write;
use std::path::Path;

pub(crate) fn write_config(dir: &Path, name: &str, contents: &str) {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
}
