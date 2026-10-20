#!/usr/bin/env python3
"""Permanent host regression tests for the reviewed lexical coverage contract."""
import copy
from pathlib import Path
import subprocess
import tempfile
import unittest

import audit


class AuditTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.path = "kernel/src/fixture.rs"
        (self.root / self.path).parent.mkdir(parents=True)

    def scan(self, source):
        (self.root / self.path).write_text(source)
        return audit.scan_sources(self.root, [self.path])[0]

    def ledger(self, sites):
        owner = {"id": "fixture.owner"}
        for key in ("purpose", "size", "segments", "mask", "lifetime", "allocation_context",
                    "release_context", "accounting", "source_status"):
            owner[key] = "reviewed fixture contract"
        owner["dynamic_evidence"] = {"status": "not_exercised", "attribution": "unknown_global_delta"}
        owner["evidence"] = [{"path": self.path, "anchor": "fn fixture"}]
        return {"schema": audit.SCHEMA, "families": dict(audit.FAMILIES),
                "owners": [owner], "sites": [dict(id=s["id"], anchor=s["anchor"],
                owner=owner["id"], disposition="fixture") for s in sites]}

    def test_nested_comments_and_all_string_forms_are_not_boundaries(self):
        sites = self.scan(r'''// PhysAddr::new(1)
/* outer allocate_page_frames(2) /* inner DmaBuffer::alloc_pages(3) */ end */
const RAW: &str = r###"PhysAddr::new(4); /* not a comment */"###;
const BYTES: &[u8] = br##"dma_alloc_pages_raw(5)"##;
const C: &CStr = cr#"Dma::new(6)"#;
const ESCAPED: &str = "\" PhysAddr::new(7)";
const CHARACTER: char = '\u{28}';
fn fixture<'a>(s: &'a str) { let value = PhysAddr::new(8); }
''')
        self.assertEqual([("physical_identity", "PhysAddr::new(8)")], [(s["family"], s["anchor"]) for s in sites])
        self.assertEqual(8, sites[0]["line"])

    def test_unterminated_tokens_fail_closed(self):
        for text in ('/* unfinished', 'r##"unfinished', '"unfinished'):
            with self.subTest(text=text), self.assertRaises(audit.AuditError):
                self.scan(text)

    def test_declarations_do_not_count_as_calls(self):
        sites = self.scan('fn dma_alloc(pages: usize) {}\nfn fixture() { dma_alloc(1); }')
        self.assertEqual(1, len(sites))
        self.assertEqual("dma_alloc(1)", sites[0]["anchor"])

    def test_multiline_generics_and_nested_invocations(self):
        sites = self.scan('fn fixture() { VirtQueue::<Hal, 256>::new_boxed(\n &mut transport, f(1, 2))?; }')
        self.assertEqual(1, len(sites))
        self.assertEqual("virtqueue", sites[0]["family"])
        self.assertTrue(sites[0]["anchor"].endswith("f(1, 2))"))

    def test_actual_aligned_box_array_and_const_generic_boundaries(self):
        source = """fn fixture() {
            AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_zeroed();
            AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_unchecked(ptr);
        }"""
        sites = self.scan(source)
        self.assertEqual(2, len(sites))
        self.assertTrue(all(s["family"] == "aligned_heap" for s in sites))
        self.assertTrue(any("new_zeroed()" in s["anchor"] for s in sites))
        self.assertTrue(any("new_unchecked(ptr)" in s["anchor"] for s in sites))

    def test_aligned_box_turbofish_does_not_consume_another_statement(self):
        sites = self.scan("""fn fixture() {
            let unused: AlignedBox::<[u8; 64], { 64 }>;
            Other::<u32>::new();
            let value = AlignedBox::<fn(u8) -> Vec<u8>, { 1 << 5 }>::new();
        }""")
        self.assertEqual(1, len(sites))
        self.assertEqual("AlignedBox::<fn(u8) -> Vec<u8>, { 1 << 5 }>::new()", sites[0]["anchor"])

    def test_parameter_change_new_boundary_and_stale_boundary_fail(self):
        original = self.scan('fn fixture() { PhysAddr::new(1); }')
        ledger = self.ledger(original)
        self.assertFalse(audit.validate(ledger, original))
        changed = self.scan('fn fixture() { PhysAddr::new(2); }')
        errors = audit.validate(ledger, changed)
        self.assertTrue(any("unclassified" in e for e in errors))
        self.assertTrue(any("stale" in e for e in errors))

    def test_argument_literals_and_token_spacing_are_not_erased(self):
        a = self.scan('fn fixture() { DmaBuffer::alloc_bytes("a b".len(), opts); }')[0]
        b = self.scan('fn fixture() { DmaBuffer::alloc_bytes("a  b".len(), opts); }')[0]
        self.assertNotEqual(a["id"], b["id"])
        a = self.scan('fn fixture() { PhysAddr::new(x as usize); }')[0]
        b = self.scan('fn fixture() { PhysAddr::new(xasusize); }')[0]
        self.assertNotEqual(a["id"], b["id"])

    def test_line_relocation_is_not_identity(self):
        a = self.scan('fn fixture() { PhysAddr::new(1); }')
        ledger = self.ledger(a)
        b = self.scan('\n\nfn fixture() { PhysAddr::new(1); }')
        self.assertNotEqual(a[0]["line"], b[0]["line"])
        self.assertFalse(audit.validate(ledger, b))

    def test_duplicate_same_call_is_not_hidden_by_hash(self):
        a = self.scan('fn fixture() { PhysAddr::new(1); }')
        b = self.scan('fn fixture() { PhysAddr::new(1); PhysAddr::new(1); }')
        self.assertEqual(2, len({s["id"] for s in b}))
        self.assertTrue(any("unclassified" in e for e in audit.validate(self.ledger(a), b)))

    def test_duplicate_owner_and_site_ids_rejected(self):
        sites = self.scan('fn fixture() { PhysAddr::new(1); }')
        ledger = self.ledger(sites)
        ledger["owners"].append(copy.deepcopy(ledger["owners"][0]))
        ledger["sites"].append(copy.deepcopy(ledger["sites"][0]))
        errors = audit.validate(ledger, sites)
        self.assertTrue(any("duplicate owner" in e for e in errors))
        self.assertTrue(any("duplicate reviewed site" in e for e in errors))

    def test_missing_contract_or_fabricated_attribution_rejected(self):
        sites = self.scan('fn fixture() { PhysAddr::new(1); }')
        ledger = self.ledger(sites)
        del ledger["owners"][0]["lifetime"]
        ledger["owners"][0]["dynamic_evidence"]["attribution"] = "exact_per_owner_from_global"
        errors = audit.validate(ledger, sites)
        self.assertTrue(any("missing lifetime" in e for e in errors))
        self.assertTrue(any("cannot identify" in e for e in errors))

    def test_unknown_owner_changed_family_or_digest_rejected(self):
        sites = self.scan('fn fixture() { PhysAddr::new(1); }')
        ledger = self.ledger(sites)
        ledger["sites"][0]["owner"] = "unreviewed"
        ledger["sites"][0]["anchor"] = "PhysAddr::new(2)"
        ledger["families"]["new_rule"] = "rule"
        errors = audit.validate(ledger, sites)
        self.assertTrue(any("unknown owner" in e for e in errors))
        self.assertTrue(any("changed reviewed" in e for e in errors))
        self.assertTrue(any("families changed" in e for e in errors))

    def test_site_identity_uses_full_digest_and_compact_review_record(self):
        sites = self.scan('fn fixture() { PhysAddr::new(1); }')
        site = sites[0]
        path, family, digest, ordinal = site["id"].rsplit(":", 3)
        self.assertEqual((self.path, "physical_identity", site["sha256"], "1"),
                         (path, family, digest, ordinal))
        self.assertEqual(64, len(digest))
        ledger = self.ledger(sites)
        self.assertEqual({"id", "anchor", "owner", "disposition"}, set(ledger["sites"][0]))
        self.assertFalse(audit.validate(ledger, sites))
        ledger["sites"][0]["path"] = self.path
        self.assertTrue(any("must contain only" in e for e in audit.validate(ledger, sites)))

    def test_only_git_tracked_kernel_rust_sources(self):
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        self.scan('fn fixture() { PhysAddr::new(1); }')
        ignored = self.root / "kernel/src/untracked.rs"
        ignored.write_text('fn fixture() { PhysAddr::new(2); }')
        outside = self.root / "outside.rs"
        outside.write_text('fn fixture() { PhysAddr::new(3); }')
        subprocess.run(["git", "-C", str(self.root), "add", self.path, "outside.rs"], check=True)
        self.assertEqual([self.path], audit.tracked_sources(self.root))
        new = "kernel/src/new.rs"
        (self.root / new).write_text('fn new_owner() { dma_alloc_pages_raw(2, opts); }')
        subprocess.run(["git", "-C", str(self.root), "add", new], check=True)
        self.assertIn(new, audit.tracked_sources(self.root))
        before, _ = audit.scan_sources(self.root, [self.path])
        after, _ = audit.scan_sources(self.root, audit.tracked_sources(self.root))
        self.assertTrue(any("unclassified" in error for error in audit.validate(self.ledger(before), after)))

    def test_stale_owner_evidence_rejected_even_when_boundary_unchanged(self):
        sites = self.scan('fn fixture() { PhysAddr::new(1); }')
        ledger = self.ledger(sites)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        subprocess.run(["git", "-C", str(self.root), "add", self.path], check=True)
        subprocess.run(["git", "-C", str(self.root), "-c", "user.name=Audit Fixture",
                        "-c", "user.email=audit@example.invalid", "commit", "-qm", "fixture"], check=True)
        self.scan('fn renamed() { PhysAddr::new(1); }')
        report = audit.audit(self.root, ledger)
        self.assertEqual("fail", report["status"])
        self.assertTrue(any("stale evidence" in e for e in report["errors"]))

    def test_malformed_ledger_is_a_validation_failure(self):
        for ledger in (None, [], {"owners": "bad", "sites": []}):
            with self.subTest(ledger=ledger):
                self.assertTrue(audit.validate(ledger, []))

    def test_source_symlink_refused(self):
        real = self.root / "real.rs"
        real.write_text("fn fixture() {}")
        (self.root / self.path).symlink_to(real)
        with self.assertRaises(audit.AuditError):
            audit.scan_sources(self.root, [self.path])

    def test_heap_signals_only_informational(self):
        (self.root / self.path).write_text('fn fixture() { let x = Vec::with_capacity(4096); }')
        sites, signals = audit.scan_sources(self.root, [self.path])
        self.assertEqual([], sites)
        self.assertEqual(1, signals["total"])
        self.assertIn("not DMA", signals["kind"])


if __name__ == "__main__":
    unittest.main()
