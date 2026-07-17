# Vendored diff fixtures

The `*.diff` files in this directory are copied from the test corpora of
existing open-source diff libraries and used here to pin wiff's unified-diff
parsing across the formats produced by git, Mercurial, Subversion, Bazaar, and
plain `diff -u`, along with a range of real-world edge cases (CRLF line endings,
miscounted hunk headers, junk text between files and hunks, and paths
containing spaces).

Some files were renamed for clarity when vendored; their contents are otherwise
unmodified. Provenance:

- `bzr.diff`, `git.diff`, `hg.diff`, `svn.diff`, `sample0.diff` through
  `sample5.diff`, `sample4-plus.diff`: from `unidiff-rs`
  (https://github.com/messense/unidiff-rs), MIT, Copyright (c) 2016 messense.
- `crlf.diff`, `miscounted-hunk-merged.diff` (originally `sample6.diff`),
  `miscounted-hunk-short.diff` (originally `sample7.diff`): from `patch-rs`
  (https://github.com/uniphil/patch-rs), MIT, Copyright (c) 2016 uniphil.
- `junk-between-files.diff`, `junk-between-hunks.diff`, `path-with-spaces.diff`
  (originally the `foo.patch` inputs of the correspondingly named compat tests),
  `added-line-looks-like-header.diff` (from the `plus_plus_content_in_hunk`
  test), `lone-file-headers.diff` (from `multi_file_mixed_headers`),
  `reversed-file-headers.diff` (from `reversed_header_order`): from `diffy`
  (https://github.com/bmwill/diffy), dual-licensed MIT OR Apache-2.0,
  Copyright (c) Brandon Williams; used here under the MIT terms.

All three projects are distributed under the MIT license, whose notice is
reproduced below.

---

The MIT License (MIT)

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
of the Software, and to permit persons to whom the Software is furnished to do
so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
