//! V4A apply_patch parser corpus.
//!
//! Each test case mirrors a real shape from the OpenAI Codex documentation
//! or one of the well-known parser bug reports (codex GH issues #2030, #15003).

use super::super::super::canonical::EditOp;
use super::parser::parse_v4a_patch;

#[test]
fn add_file_simple() {
    let patch = "\
*** Begin Patch
*** Add File: foo.py
+def f():
+    return 1
*** End Patch";
    let ops = parse_v4a_patch(patch).expect("should parse");
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        EditOp::Create { path, file_text } => {
            assert_eq!(path, "foo.py");
            assert_eq!(file_text, "def f():\n    return 1");
        }
        other => panic!("expected Create, got {other:?}"),
    }
}

#[test]
fn delete_file_simple() {
    let patch = "\
*** Begin Patch
*** Delete File: old.py
*** End Patch";
    let ops = parse_v4a_patch(patch).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(&ops[0], EditOp::Delete { path } if path == "old.py"));
}

#[test]
fn update_file_single_hunk_with_context() {
    let patch = "\
*** Begin Patch
*** Update File: bar.py
@@ class Greeter
     def hello(self):
-        print(\"hi\")
+        print(\"hello\")
*** End Patch";
    let ops = parse_v4a_patch(patch).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        EditOp::StrReplace {
            path,
            old_str,
            new_str,
        } => {
            assert_eq!(path, "bar.py");
            assert_eq!(old_str, "    def hello(self):\n        print(\"hi\")");
            assert_eq!(new_str, "    def hello(self):\n        print(\"hello\")");
        }
        other => panic!("expected StrReplace, got {other:?}"),
    }
}

#[test]
fn update_file_multiple_hunks() {
    let patch = "\
*** Begin Patch
*** Update File: lib.rs
@@ fn one
-    let x = 1;
+    let x = 10;
@@ fn two
-    let y = 2;
+    let y = 20;
*** End Patch";
    let ops = parse_v4a_patch(patch).unwrap();
    assert_eq!(ops.len(), 2);
    match (&ops[0], &ops[1]) {
        (
            EditOp::StrReplace {
                path: p1,
                old_str: o1,
                new_str: n1,
            },
            EditOp::StrReplace {
                path: p2,
                old_str: o2,
                new_str: n2,
            },
        ) => {
            assert_eq!(p1, "lib.rs");
            assert_eq!(p2, "lib.rs");
            assert_eq!(o1, "    let x = 1;");
            assert_eq!(n1, "    let x = 10;");
            assert_eq!(o2, "    let y = 2;");
            assert_eq!(n2, "    let y = 20;");
        }
        _ => panic!("expected two StrReplace"),
    }
}

#[test]
fn update_with_move_emits_rename() {
    let patch = "\
*** Begin Patch
*** Update File: src/old.py
*** Move to: src/new.py
@@ def f
-    return 1
+    return 2
*** End Patch";
    let ops = parse_v4a_patch(patch).unwrap();
    assert_eq!(ops.len(), 2);
    match &ops[0] {
        EditOp::StrReplace { path, .. } => assert_eq!(path, "src/old.py"),
        other => panic!("expected StrReplace first, got {other:?}"),
    }
    match &ops[1] {
        EditOp::Rename { from, to } => {
            assert_eq!(from, "src/old.py");
            assert_eq!(to, "src/new.py");
        }
        other => panic!("expected Rename last, got {other:?}"),
    }
}

#[test]
fn mixed_add_update_delete() {
    let patch = "\
*** Begin Patch
*** Add File: a.py
+x = 1
*** Update File: b.py
@@ def f
-    return 1
+    return 2
*** Delete File: c.py
*** End Patch";
    let ops = parse_v4a_patch(patch).unwrap();
    assert_eq!(ops.len(), 3);
    assert!(matches!(&ops[0], EditOp::Create { path, .. } if path == "a.py"));
    assert!(matches!(&ops[1], EditOp::StrReplace { path, .. } if path == "b.py"));
    assert!(matches!(&ops[2], EditOp::Delete { path } if path == "c.py"));
}

#[test]
fn end_of_file_marker_is_handled() {
    let patch = "\
*** Begin Patch
*** Update File: tail.py
@@ def main
     pass
-    return None
+    return 0
*** End of File
*** End Patch";
    let ops = parse_v4a_patch(patch).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        EditOp::StrReplace {
            old_str, new_str, ..
        } => {
            assert!(old_str.contains("return None"));
            assert!(new_str.contains("return 0"));
            assert!(!old_str.contains("End of File"));
        }
        _ => panic!("expected StrReplace"),
    }
}

#[test]
fn crlf_line_endings_are_normalised() {
    let patch = "*** Begin Patch\r\n*** Add File: x.txt\r\n+hello\r\n*** End Patch\r\n";
    let ops = parse_v4a_patch(patch).unwrap();
    match &ops[0] {
        EditOp::Create { file_text, .. } => assert_eq!(file_text, "hello"),
        _ => panic!("expected Create"),
    }
}

#[test]
fn leading_and_trailing_whitespace_tolerated() {
    let patch = "\n\n  *** Begin Patch\n*** Add File: x\n+y\n*** End Patch  \n\n";
    parse_v4a_patch(patch).unwrap();
}

#[test]
fn missing_begin_envelope_errors() {
    let patch = "*** Add File: foo\n+bar\n*** End Patch";
    assert!(parse_v4a_patch(patch).is_err());
}

#[test]
fn missing_end_envelope_errors() {
    let patch = "*** Begin Patch\n*** Add File: foo\n+bar\n";
    assert!(parse_v4a_patch(patch).is_err());
}

#[test]
fn add_file_body_must_be_prefixed_with_plus() {
    let patch = "\
*** Begin Patch
*** Add File: foo
not prefixed
*** End Patch";
    assert!(parse_v4a_patch(patch).is_err());
}

#[test]
fn update_file_without_hunk_header_errors() {
    let patch = "\
*** Begin Patch
*** Update File: bar.py
-    return 1
+    return 2
*** End Patch";
    assert!(parse_v4a_patch(patch).is_err());
}

#[test]
fn update_file_without_hunks_errors() {
    let patch = "\
*** Begin Patch
*** Update File: bar.py
*** End Patch";
    assert!(parse_v4a_patch(patch).is_err());
}

#[test]
fn empty_patch_errors() {
    let patch = "*** Begin Patch\n*** End Patch";
    assert!(parse_v4a_patch(patch).is_err());
}

#[test]
fn unexpected_line_outside_file_section_errors() {
    let patch = "\
*** Begin Patch
random text
*** Add File: x
+y
*** End Patch";
    assert!(parse_v4a_patch(patch).is_err());
}

#[test]
fn add_file_preserves_blank_lines_in_body() {
    let patch = "\
*** Begin Patch
*** Add File: x.py
+a

+b
*** End Patch";
    let ops = parse_v4a_patch(patch).unwrap();
    match &ops[0] {
        EditOp::Create { file_text, .. } => assert_eq!(file_text, "a\n\nb"),
        _ => panic!("expected Create"),
    }
}

#[test]
fn hunk_with_multiple_context_and_changes() {
    let patch = "\
*** Begin Patch
*** Update File: f.py
@@ def f
     a = 1
     b = 2
-    c = 3
+    c = 30
+    d = 4
     e = 5
*** End Patch";
    let ops = parse_v4a_patch(patch).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        EditOp::StrReplace {
            old_str, new_str, ..
        } => {
            assert_eq!(old_str, "    a = 1\n    b = 2\n    c = 3\n    e = 5");
            assert_eq!(
                new_str,
                "    a = 1\n    b = 2\n    c = 30\n    d = 4\n    e = 5"
            );
        }
        _ => panic!("expected StrReplace"),
    }
}
