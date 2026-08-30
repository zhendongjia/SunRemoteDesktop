# Roadmap

## Phase 1 — Windows interactive MVP

- [x] SunRDP RDP/TLS listener.
- [x] Full-frame desktop update pipeline.
- [x] Keyboard and mouse injection through Win32.
- [x] Local account validation and allow-list management.
- [x] Cross-platform configuration and GUI shell.
- [ ] Interoperability matrix for mstsc, FreeRDP, Remmina and other clients.

## Phase 2 — Windows service and session bridge

- [x] Move Windows capture and input into a service-managed physical-console agent.
- [x] Add a named-pipe protocol with versioning, frame back-pressure and ACL checks.
- [x] Follow active-console Session changes and `default`/`winlogon`/`screensaver` input-desktop changes.
- [x] Start the physical-console agent from the service without requiring an interactive logon.
- [ ] Add service status and agent status to the administration UI.
- [x] Capture the physical console's login, lock and UAC input desktops through a LocalSystem helper without weakening Windows security boundaries.

## Phase 3 — Shared capabilities

- [ ] Read-only mode and per-user control policy.
- [x] Single-monitor client-sized access UI, explicit scaling and primary-display matching.
- [x] Single-monitor dynamic Display Control resizing with per-connection scale/match policy.
- [x] MS-RDPEI primary-contact input for direct-touch taps and single-finger drags.
- [ ] Native multi-touch gestures and pen forwarding.
- [ ] Multiple-monitor Display Control layouts.
- [ ] Clipboard, audio and optional file-transfer channels, each disabled by default.
- [ ] Certificate import, rotation, fingerprint display and audit logs.
- [ ] Connection metrics, frame pacing and adaptive quality.

## Phase 4 — Linux and macOS

- [ ] Linux X11 capture/input backend.
- [ ] Linux Wayland portal and PipeWire backend with visible user authorization.
- [ ] macOS ScreenCaptureKit and Accessibility backend.
- [ ] Per-platform authentication providers while keeping the shared authorization model.
- [ ] CI builds for Windows, Linux X11, Linux Wayland and macOS.

New features should be added as capabilities behind explicit configuration switches. The SunRDP core should receive typed display/input/channel interfaces rather than platform-specific conditionals.
