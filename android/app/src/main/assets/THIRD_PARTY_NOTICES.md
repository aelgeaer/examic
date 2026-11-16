# Third-party notices

Examic is MIT OR Apache-2.0. It ships the models and libraries below, each
under its own license.

## Models

| Model | License | Source |
|-------|---------|--------|
| GigaAM v3 end-to-end CTC (speech recognition), Q8_0 GGUF | MIT | ai-sage/GigaAM-v3, converted to GGUF by handy-computer (huggingface.co/handy-computer/gigaam-v3-e2e-ctc-gguf) |
| opus-mt_tiny_rus-eng (translation) | Apache-2.0 | Helsinki-NLP/opus-mt_tiny_rus-eng, converted to int8 ONNX by Examic's tools/models/convert_opus_mt.py |

**GigaAM v3** — MIT License, Copyright (c) 2024 GigaChat Team.
Modifications: handy-computer converted https://huggingface.co/ai-sage/GigaAM-v3
at commit cec030b to GGUF for transcribe.cpp. Conversion repackages the
weights, renaming and re-laying-out tensors; the quantized files also store
them at reduced numeric precision. The weights were not retrained or
fine-tuned.

**opus-mt_tiny_rus-eng** — Copyright Language Technology Research Group at the
University of Helsinki. Licensed under the Apache License, Version 2.0
(https://www.apache.org/licenses/LICENSE-2.0). Modifications: exported to
ONNX and quantized to 8-bit integers; the weights were not retrained.

## Native libraries

| Library | License | Copyright |
|---------|---------|-----------|
| transcribe.cpp | MIT | The transcribe.cpp authors |
| ggml (in transcribe.cpp) | MIT | The ggml authors |
| miniz (in transcribe.cpp) | MIT | Rich Geldreich, RAD Game Tools, Valve Software |
| Silero VAD code (in transcribe.cpp) | MIT | Silero Team |
| ONNX Runtime | MIT | Microsoft Corporation |
| libopus | BSD-3-Clause | Xiph.Org Foundation, Skype Limited, Octasic, Jean-Marc Valin, Timothy B. Terriberry, CSIRO, Gregory Maxwell, Mark Borgerding, Erik de Castro Lopo |
| SQLite | Public domain | — |
| Rust crates (rusqlite, ogg, opus, ort, uniffi, zip, …) | MIT and/or Apache-2.0 | Their authors |
| AndroidX, Jetpack Compose, CameraX, Media3 | Apache-2.0 | The Android Open Source Project |
| JNA | Apache-2.0 | Timothy Wall and contributors |

### MIT License

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

### BSD 3-Clause License (libopus)

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

- Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.
- Redistributions in binary form must reproduce the above copyright notice,
  this list of conditions and the following disclaimer in the documentation
  and/or other materials provided with the distribution.
- Neither the name of Internet Society, IETF or IETF Trust, nor the names of
  specific contributors, may be used to endorse or promote products derived
  from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

### Apache License 2.0

The full text is at https://www.apache.org/licenses/LICENSE-2.0 and in this
repository as LICENSE-APACHE.
