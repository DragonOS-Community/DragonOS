use alloc::string::String;

pub(crate) fn escape_mount_token(input: &str, escape_hash: bool) -> String {
    escape_proc_field(input, escape_hash)
}

pub(crate) fn escape_path_token(input: &str) -> String {
    escape_proc_field(input, false)
}

fn escape_proc_field(input: &str, escape_hash: bool) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            ' ' => escaped.push_str("\\040"),
            '\t' => escaped.push_str("\\011"),
            '\n' => escaped.push_str("\\012"),
            '\\' => escaped.push_str("\\134"),
            '#' if escape_hash => escaped.push_str("\\043"),
            _ => escaped.push(ch),
        }
    }
    escaped
}
