// Copyright 2018 astonbitecode
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use libc::c_char;
use std::ffi::{CStr, CString, OsStr};
use std::{str, self};
use crate::errors;

pub fn to_rust_string(pointer: *const c_char) -> String {
    let slice = unsafe { CStr::from_ptr(pointer).to_bytes() };
    str::from_utf8(slice).unwrap().to_string()
}

pub fn to_java_string(string: &str) -> *mut c_char {
    let cs = CString::new(string.as_bytes()).unwrap();
    cs.into_raw()
}

#[cfg(not(target_os = "windows"))]
pub fn classpath_sep() -> &'static str {
    ":"
}

#[cfg(target_os = "windows")]
pub fn classpath_sep() -> &'static str {
    ";"
}

pub fn java_library_path() -> errors::Result<String> {
    let default = format!("-Djava.library.path={}", deps_dir()?);
    if cfg!(windows) {
        Ok(default)
    } else {
        Ok(format!("{}:/usr/lib:/lib", default))
    }
}

pub fn deps_dir() -> errors::Result<String> {
    let mut deps_fallback = std::env::current_exe()?;
    deps_fallback.pop();

    if deps_fallback.file_name() == Some(OsStr::new("deps")) {
        deps_fallback.pop();
    }

    deps_fallback.push("deps");

    Ok(deps_fallback
        .to_str()
        .unwrap_or("./deps/").to_owned())
}
