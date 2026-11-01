# Protocol reference

The experimental cloud replay client adapts authentication, request framing,
and build-ID calculations from [dank/rlapi](https://github.com/dank/rlapi),
reviewed October 31, 2026. It uses only device authorization, token refresh,
player authentication and the authenticated player's match history. It does
not use the project's instrumentation, interception, or game-hooking tools.

MIT License

Copyright (c) 2025 dank

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

The Rust executable uses egui/eframe, reqwest, rusqlite/SQLite, rustls, ring,
tungstenite, Windows bindings, and the dependencies recorded in `Cargo.lock`.
Package versions, license declarations, and bundled license texts are collected
in [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt). Include both notice files
when distributing the executable. Segoe UI is loaded from the user's Windows
installation; no Microsoft font is bundled with the app.
