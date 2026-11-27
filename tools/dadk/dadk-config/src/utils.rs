// SPDX-License-Identifier: MPL-2.0
// Note: This file is derived from the asterinas osdk,
// and we extend our heartfelt thanks to the developers of Asterinas!

//! This module contains utilities for manipulating common Unix command-line arguments.

use anyhow::{anyhow, Result};
use indexmap::{IndexMap, IndexSet};

/// Split a string of Unix arguments into an array of key-value strings or switches.
/// Positional arguments are not supported.
pub fn split_to_kv_array(args: &str) -> Result<Vec<String>> {
    let target = shlex::split(args).ok_or(anyhow!("Failed to split unix arguments"))?;

    // Join the key value arguments as a single element
    let mut joined = Vec::<String>::new();
    let mut last_has_value = false;
    for elem in target {
        if !elem.starts_with('-') && !last_has_value {
            if let Some(last) = joined.last_mut() {
                last.push(' ');
                last.push_str(&elem);
                last_has_value = true;
                continue;
            }
        }
        joined.push(elem);
        last_has_value = false;
    }

    Ok(joined)
}

/// Apply key-value pairs to an array of strings.
///
/// The provided arguments will be appended to the array if the key is not already present or if the key is a multi-value key.
/// Otherwise, the value will be updated.
pub fn apply_kv_array(
    array: &mut Vec<String>,
    args: &Vec<String>,
    separator: &str,
    multi_value_keys: &[&str],
    single_value_keys: &[&str],
) -> Result<()> {
    let multi_value_keys = {
        let mut inferred_keys = infer_multi_value_keys(array, separator);
        for key in multi_value_keys {
            inferred_keys.insert(key.to_string());
        }
        inferred_keys
    };

    log::debug!("apply_kv_array: multi value keys: {:?}", multi_value_keys);

    // We use IndexMap to keep key orders
    let mut key_strings = IndexMap::new();
    let mut multi_value_key_strings: IndexMap<String, Vec<String>> = IndexMap::new();
    for item in array.drain(..) {
        // Each key-value string has two patterns:
        // 1. Separated by separator: key value / key=value
        if let Some(key) = get_key(&item, separator) {
            if multi_value_keys.contains(&key) {
                if let Some(v) = multi_value_key_strings.get_mut(&key) {
                    v.push(item);
                } else {
                    let v = vec![item];
                    multi_value_key_strings.insert(key, v);
                }
                continue;
            }

            key_strings.insert(key, item);
            continue;
        }
        // 2. Only key, no value
        key_strings.insert(item.clone(), item);
    }

    for arg in args {
        if let Some(key) = get_key(arg, separator) {
            if multi_value_keys.contains(&key) {
                if let Some(v) = multi_value_key_strings.get_mut(&key) {
                    v.push(arg.to_owned());
                } else {
                    let v = vec![arg.to_owned()];
                    multi_value_key_strings.insert(key, v);
                }
                continue;
            }

            key_strings.insert(key, arg.to_owned());
            continue;
        } else {
            if single_value_keys.contains(&arg.as_str()) || multi_value_keys.contains(arg) {
                return Err(anyhow!("Invalid argument: {}", arg));
            } else {
                key_strings.insert(arg.to_owned(), arg.to_owned());
            }
        }
    }

    *array = key_strings.into_iter().map(|(_, value)| value).collect();

    for (_, mut values) in multi_value_key_strings {
        array.append(&mut values);
    }
    Ok(())
}

fn infer_multi_value_keys(array: &Vec<String>, separator: &str) -> IndexSet<String> {
    let mut multi_val_keys = IndexSet::new();

    let mut occurred_keys = IndexSet::new();
    for item in array {
        let Some(key) = get_key(item, separator) else {
            continue;
        };

        if occurred_keys.contains(&key) {
            multi_val_keys.insert(key);
        } else {
            occurred_keys.insert(key);
        }
    }

    multi_val_keys
}

pub fn get_key(item: &str, separator: &str) -> Option<String> {
    let split = item.split(separator).collect::<Vec<_>>();
    let len = split.len();
    if len > 2 || len == 0 {
        log::error!("{} is an invalid argument.", item);
        return None;
    }

    if len == 1 {
        return None;
    }

    let key = split.first().unwrap();

    Some(key.to_string())
}

#[cfg(test)]
pub mod test {
    use super::*;

    #[test]
    fn test_get_key() {
        let string1 = "init=/bin/init";
        let key = get_key(string1, "=").unwrap();
        assert_eq!(key.as_str(), "init");

        let string2 = "-m 8G";
        let key = get_key(string2, " ").unwrap();
        assert_eq!(key.as_str(), "-m");

        let string3 = "-device virtio-keyboard-pci,disable-legacy=on,disable-modern=off";
        let key = get_key(string3, " ").unwrap();
        assert_eq!(key.as_str(), "-device");

        let string4 = "-device";
        assert!(get_key(string4, " ").is_none());

        let string5 = "-m 8G a";
        assert!(get_key(string5, " ").is_none());
        let string6 = "-m=8G";
        assert!(get_key(string6, " ").is_none());
    }

    #[test]
    fn test_apply_kv_array() {
        let qemu_args = &[
            "-enable-kvm",
            "-m 8G",
            "-device virtio-blk-pci,bus=pcie.0,addr=0x6,drive=x0,disable-legacy=on,disable-modern=off",
            "-device virtio-keyboard-pci,disable-legacy=on,disable-modern=off",
        ];

        let args = &["-m 100G", "-device ioh3420,id=pcie.0,chassis=1"];

        let expected = &[
            "-enable-kvm",
            "-m 100G",
            "-device virtio-blk-pci,bus=pcie.0,addr=0x6,drive=x0,disable-legacy=on,disable-modern=off",
            "-device virtio-keyboard-pci,disable-legacy=on,disable-modern=off",
            "-device ioh3420,id=pcie.0,chassis=1",
        ];

        let mut array = qemu_args.iter().map(ToString::to_string).collect();
        let args = args.iter().map(ToString::to_string).collect();
        assert!(apply_kv_array(&mut array, &args, " ", &["-device"], &[]).is_ok());

        let expected: Vec<_> = expected.iter().map(ToString::to_string).collect();
        assert_eq!(expected, array);
    }

    #[test]
    fn test_apply_kv_array_insert_multi_value_key() {
        let mut array = vec!["key1=value1".to_string(), "key2=value2".to_string()];
        let args = vec!["key3=value3".to_string()];
        let separator = "=";
        let multi_value_keys = vec!["key3"];
        let single_value_keys: Vec<&str> = vec![];

        let result = apply_kv_array(
            &mut array,
            &args,
            separator,
            &multi_value_keys,
            &single_value_keys,
        );

        assert!(result.is_ok());
        assert_eq!(
            array,
            vec![
                "key1=value1".to_string(),
                "key2=value2".to_string(),
                "key3=value3".to_string(),
            ]
        );
    }
}
