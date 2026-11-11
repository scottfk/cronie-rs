//! cronie-rs: a drop-in Rust port of cronie 1.7.2.
//!
//! The library holds what upstream's programs share (upstream links the
//! same object files into crond, crontab and cronnext); the programs are in
//! `src/bin/`.  Module map:
//!
//! * [`entry`]: src/entry.c, one crontab line
//! * [`env`]: src/env.c, environment settings
//! * [`user`]: src/user.c and the loading half of src/database.c
//! * [`reader`]: stdio input as the parser uses it, and misc.c's helpers
//! * [`globals`]: globals.h, `log_it()`, debug flags
//! * [`random`]: glibc's `random()`
//! * [`sys`]: every libc call (the only `unsafe` code)
//! * [`stdio`], [`getopt`], [`num`], [`bytes`]: glibc behavior the output
//!   depends on (copied from openrc-rs/metalog-rs)

pub mod bytes;
pub mod entry;
pub mod env;
#[allow(dead_code)]
pub mod getopt;
pub mod globals;
#[allow(dead_code)]
pub mod num;
pub mod random;
pub mod reader;
pub mod stdio;
pub mod sys;
pub mod user;

/// `PACKAGE_STRING`.
pub const PACKAGE_STRING: &str = "cronie 1.7.2";
