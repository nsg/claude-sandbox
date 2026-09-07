# Vendored noVNC

This directory contains files from the noVNC repository at
<https://github.com/novnc/noVNC>, tag `v1.7.0`.

Included from that release:

- all files under `core/`;
- all files under `vendor/pako/`, including `vendor/pako/LICENSE`;
- `LICENSE.txt`;
- `AUTHORS`; and
- `docs/LICENSE.MPL-2.0`.

The upstream files are unmodified. `VENDOR.md` is maintained by this project.

To update, download the archive for the desired upstream tag, replace only the
paths listed above with their contents from the extracted archive, confirm no
HTML, CSS, application, image, font, test, or other documentation files were
introduced, update the tag in this file, and run the Rust test suite.
