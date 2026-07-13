//! Build: discover PHP via php-config, compile the embed shim, link libphp.
//!
//! Per-version builds (buran-php83/84/85) set BURAN_PHP_CONFIG to the
//! matching php-config; the default takes whatever is in PATH.
//! BURAN_PHP_LIB overrides the library link name and BURAN_PHP_LIB_DIR the
//! search directory: distros shuffle both across versions (Alpine has had
//! /usr/lib/libphp83.so and /usr/lib/php85/libphp.so), while the official
//! php images ship plain {prefix}/lib/libphp.so — the defaults.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=BURAN_PHP_CONFIG");
    println!("cargo:rerun-if-env-changed=BURAN_PHP_LIB");
    println!("cargo:rerun-if-env-changed=BURAN_PHP_LIB_DIR");
    println!("cargo:rerun-if-changed=src/embed_shim.c");
    println!("cargo:rerun-if-changed=src/sapi_shim.c");

    let php_config =
        std::env::var("BURAN_PHP_CONFIG").unwrap_or_else(|_| "php-config".to_string());
    let php_lib = std::env::var("BURAN_PHP_LIB").unwrap_or_else(|_| "php".to_string());

    let includes = php_config_output(&php_config, "--includes");
    let prefix = php_config_output(&php_config, "--prefix");
    let version = php_config_output(&php_config, "--version");

    let mut build = cc::Build::new();
    build.file("src/embed_shim.c");
    build.file("src/sapi_shim.c");
    for flag in includes.split_whitespace() {
        build.flag(flag);
    }
    build.compile("buran_php_embed_shim");

    let lib_dir = std::env::var("BURAN_PHP_LIB_DIR")
        .unwrap_or_else(|_| format!("{}/lib", prefix.trim()));
    println!("cargo:rustc-link-search=native={lib_dir}");
    println!("cargo:rustc-link-lib=dylib={php_lib}");
    println!("cargo:rustc-env=BURAN_PHP_VERSION={}", version.trim());
}

fn php_config_output(php_config: &str, arg: &str) -> String {
    let out = Command::new(php_config)
        .arg(arg)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {php_config} {arg}: {e}; install php or set BURAN_PHP_CONFIG"));
    assert!(out.status.success(), "{php_config} {arg} failed");
    String::from_utf8(out.stdout).expect("php-config output is not utf-8")
}
