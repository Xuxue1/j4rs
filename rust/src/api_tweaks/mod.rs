use std::os::raw::c_void;

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
use jni_sys::{JavaVM, jclass, jint, JNIEnv, jsize};

#[cfg(not(any(target_os = "android", target_os = "macos")))]
mod generic;

#[cfg(not(any(target_os = "android", target_os = "macos")))]
pub fn get_created_java_vms(vm_buf: &mut Vec<*mut JavaVM>, buf_len: jsize, n_vms: *mut jsize) -> jint {
    generic::get_created_java_vms(vm_buf, buf_len, n_vms)
}

#[cfg(not(any(target_os = "android", target_os = "macos")))]
pub fn set_java_vm(_: *mut JavaVM) {}

#[cfg(not(any(target_os = "android", target_os = "macos")))]
pub fn create_java_vm(
    pvm: *mut *mut JavaVM,
    penv: *mut *mut c_void,
    args: *mut c_void,
) -> jint { generic::create_java_vm(pvm, penv, args) }

#[cfg(not(any(target_os = "android", target_os = "macos")))]
pub fn find_class(env: *mut JNIEnv, classname: &str) -> jclass {
    generic::find_class(env, classname)
}

// ++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++ //

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
pub fn get_created_java_vms(vm_buf: &mut Vec<*mut JavaVM>, buf_len: jsize, n_vms: *mut jsize) -> jint {
    android::get_created_java_vms(vm_buf, buf_len, n_vms)
}

#[cfg(target_os = "android")]
pub fn set_java_vm(java_vm: *mut JavaVM) {
    android::set_java_vm(java_vm);
}

#[cfg(target_os = "android")]
pub fn create_java_vm(
    pvm: *mut *mut JavaVM,
    penv: *mut *mut c_void,
    args: *mut c_void,
) -> jint { android::create_java_vm(pvm, penv, args) }

#[cfg(target_os = "android")]
pub fn find_class(env: *mut JNIEnv, classname: &str) -> jclass {
    android::find_class(env, classname)
}

// ++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++ //

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub fn get_created_java_vms(vm_buf: &mut Vec<*mut JavaVM>, buf_len: jsize, n_vms: *mut jsize) -> jint {
    macos::get_created_java_vms(vm_buf, buf_len, n_vms)
}

#[cfg(target_os = "macos")]
pub fn set_java_vm(_: *mut JavaVM) {}

#[cfg(target_os = "macos")]
pub fn create_java_vm(
    pvm: *mut *mut JavaVM,
    penv: *mut *mut c_void,
    args: *mut c_void,
) -> jint { macos::create_java_vm(pvm, penv, args) }

#[cfg(target_os = "macos")]
pub fn find_class(env: *mut JNIEnv, classname: &str) -> jclass {
    macos::find_class(env, classname)
}
