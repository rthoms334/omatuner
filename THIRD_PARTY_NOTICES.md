# Third-party notices

Tuner is licensed under the GNU General Public License, version 3 or later (see `LICENSE`).
Parts of it are ported from the projects below, whose licences require this notice.

## pyatv

Tuner's AirPlay pairing and session code (DESIGN §40 §7) is ported from
[pyatv](https://github.com/postlund/pyatv) at commit
`b277a4c8222ecdcbaab8a24e3e713ca44765adb4` (release 0.18.0), and from the open pyatv pull
requests 2846, 2894 and 2899 and the `codex/play-url-tvos26` branch of robkochman/pyatv, which
are contributions to pyatv under the same licence. Every file that carries ported code starts
with a `// Ported from pyatv <path> @ b277a4c8 (MIT, © 2020 Pierre Ståhl).` line naming its
source: the AirPlay client under `src/cast/airplay/` (TLV8, SRP-6a, the HAP frame cipher,
pair-setup and pair-verify, the HTTP/RTSP transport, the session and its event channel) and the
test fake receiver `tests/support/fake_airplay.rs`. The test vectors in
`tests/fixtures/cast/vectors/pyatv_0_18_0.json` were produced by pyatv 0.18.0 itself
(`scripts/cast/pyatv_vectors.py`).

```
The MIT License (MIT)

Copyright (c) 2020 Pierre Ståhl

Permission is hereby granted, free of charge, to any person obtaining a copy of this software
and associated documentation files (the "Software"), to deal in the Software without
restriction, including without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

Protocol facts (header and key names, constants, step order, error codes, test scenarios) were
also read from foxCast, doubletake, VioletRelay, LIVI, castcli and the openairplay notes; no
code from them is included (DESIGN §40 §7.0, licence policy).
