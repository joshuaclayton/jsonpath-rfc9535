// One query per execution-plan shape the splitter produces. Single-sourced (via
// `include!`) into both differential harnesses — `tests/scan_properties.rs` and
// `fuzz/fuzz_targets/scan_differential.rs` — so the two pools cannot drift: a plan
// shape added here is property-tested and fuzzed by the same edit.
const QUERIES: &[&str] = &[
    "$.a.b",              // singular full scan
    "$..a",               // descendant full scan (overlap possible → adaptive budget)
    "$.a[*].b",           // wildcard full scan
    "$.a[0]",             // index full scan
    "$.a[?@.n > 3]",      // pushdown, comparison leaf
    "$.a[?@.n].b",        // pushdown + residual
    "$.a[?@.n && @.x]",   // pushdown, two leaves
    "$.a[?@.b && @.b.k]", // pushdown, overlapping leaves (parent + extension)
    "$[?@.x]",            // pushdown at the root
    "$.a[?@.b[*]]",       // plain scan with predicate (general existence)
    "$..a[?@.n]",         // plain scan with predicate (descendant prefix)
    "$.a[?@ > 1]",        // plain scan with predicate (bare `@`)
    "$.a[1:3]",           // slice in the residual
    "$.a[-1]",            // negative index in the residual
    "$[-1]",              // unsplittable: DOM fallback in every mode
    "$[*]",               // whole-document: adaptive routes to DOM, forced scans
];
