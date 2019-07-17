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

use serde_json;
use std::{fmt, result};
use std::error::Error;
use std::ffi::NulError;
use std::io;
use fs_extra;
use std::sync::{TryLockError, PoisonError};

pub type Result<T> = result::Result<T, J4RsError>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum J4RsError {
    GeneralError(String),
    JavaError(String),
    JniError(String),
    RustError(String),
    ParseError(String),
}

impl fmt::Display for J4RsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &J4RsError::GeneralError(ref message) => write!(f, "{}", message),
            &J4RsError::JavaError(ref message) => write!(f, "{}", message),
            &J4RsError::JniError(ref message) => write!(f, "{}", message),
            &J4RsError::RustError(ref message) => write!(f, "{}", message),
            &J4RsError::ParseError(ref message) => write!(f, "{}", message),
        }
    }
}

impl Error for J4RsError {
    fn description(&self) -> &str {
        match *self {
            J4RsError::GeneralError(_) => "A general error occured",
            J4RsError::JavaError(_) => "An error coming from Java occured",
            J4RsError::JniError(_) => "A JNI error occured",
            J4RsError::RustError(_) => "An error coming from Rust occured",
            J4RsError::ParseError(_) => "A parsing error occured",
        }
    }
}

impl From<NulError> for J4RsError {
    fn from(err: NulError) -> J4RsError {
        J4RsError::JniError(format!("{:?}", err))
    }
}

impl From<io::Error> for J4RsError {
    fn from(err: io::Error) -> J4RsError {
        J4RsError::GeneralError(format!("{:?}", err))
    }
}

impl From<serde_json::Error> for J4RsError {
    fn from(err: serde_json::Error) -> J4RsError {
        J4RsError::ParseError(format!("{:?}", err))
    }
}

impl From<fs_extra::error::Error> for J4RsError {
    fn from(err: fs_extra::error::Error) -> J4RsError {
        J4RsError::GeneralError(format!("{:?}", err))
    }
}

impl <T> From<TryLockError<T>> for J4RsError {
    fn from(err:TryLockError<T>) -> J4RsError {
        J4RsError::GeneralError(format!("{:?}", err))
    }
}

impl <T> From<PoisonError<T>> for J4RsError {
    fn from(err:PoisonError<T>) -> J4RsError {
        J4RsError::GeneralError(format!("{:?}", err))
    }
}