# Brief: macOS and Windows support for a gpui-based Rust app (state of the art, late 2025/2026)

gertie is a real Rust workspace using gpui-kit (Longbridge's gpui-pre namespace over Zed's
gpui, gpui-component 0.7.x), currently Linux-first (Wayland/X11). The DESIGN.md identity row
says: Linux first, macOS second, no Windows in v1 (documented, not half-supported). The user
asks: is macOS and Windows actually feasible with just Rust and gpui? We need a current,
evidence-based answer, not vibes.

Research these specific questions:

1. **gpui's platform backends in 2025/2026**: what platforms does Zed's gpui actually support
   today? macOS (Metal), Linux (Wayland/X11 viablade? DirectX?), Windows status — search for
   the Windows support PRs/commits (there was a GSoC 2025 project for Windows support; check
   whether it landed, what works, what doesn't: IME, key bindings, renderer, winit migration?).
   Check the gpui-kit fork (Longbridge) specifically: does it track upstream Windows work or
   diverge? Find the actual PR numbers/issues.
2. **Zed on Windows reality check**: is Zed itself shipping a Windows build? What's the
   official statement, what's actually blocking (there were blog posts / issue threads). This
   is the best real-world signal for "how hard is gpui on Windows" since Zed is the largest
   gpui consumer.
3. **Longbridge's own products**: gpui-kit exists because Longbridge ships production apps
   with it — which platforms do THEY ship? If they ship macOS and/or Windows with this
   stack, that's the strongest existence proof. Look for their app releases (Longbridge
   Pro? whale? their terminal?) and any blog posts.
4. **macOS specifics for a gpui app**: Metal via wgpu? What does packaging/signing/notarization
   look like for a Rust+gpui app (cargo-bundle? Tauri-style bundling?), and what does Zed's
   own mac build setup suggest? Any known macOS-only gaps in gpui-kit 0.7.x?
5. **Windows specifics**: DirectX vs Vulkan vs software rendering in gpui's Windows path,
   input method handling, and whether any non-Zed production app ships gpui on Windows.
6. **Alternatives if gpui stalls**: what would a macOS/Windows port cost via (a) waiting for
   upstream gpui Windows work, (b) using egui/iced for a second frontend (gertie's core is
   UI-free by design, so a second frontend is architecturally possible), (c) web/Tauri
   wrapper. Only brief; the question is mainly about gpui's actual reach.
7. **CI**: what does cross-platform CI for gpui apps look like (GitHub Actions runners for
   mac/win, Linux headless testing of gpui apps — gertie's test harness runs gpui in
   test-support mode without a display, does that work on mac/windows runners?).

Constraints: primary sources only (Zed's GitHub, Longbridge's repos/blogs, gpui CHANGELOGs,
actual PRs). Report with URLs. Flag uncertainty honestly: if Windows gpui status is "in
progress, partially working," say exactly what works and what doesn't as of the latest
commits. Do not pad. Under ~15 tool rounds; prioritize finding the Windows support status
and Longbridge's shipped platforms first, those two answers decide the recommendation.
