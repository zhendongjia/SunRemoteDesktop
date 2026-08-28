# Roadmap

## Phase 1 — Windows interactive MVP

- [x] RDP/TLS listener based on IronRDP.
- [x] Full-frame desktop update pipeline.
- [x] Keyboard and mouse injection through Win32.
- [x] Local account validation and allow-list management.
- [x] Cross-platform configuration and GUI shell.
- [ ] Interoperability matrix for mstsc, FreeRDP, Remmina and other clients.

## Phase 2 — Windows service and session bridge

- [ ] Move Windows capture and input into a per-session agent.
- [ ] Add a named-pipe protocol with versioning, frame back-pressure and ACL checks.
- [ ] Let the service track logon/logoff/session-lock events.
- [ ] Start the agent automatically for every permitted interactive session.
- [ ] Add service status and agent status to the administration UI.
- [ ] Decide explicitly how secure desktop and UAC prompts are handled; do not silently weaken Windows security boundaries.

## Phase 3 — Shared capabilities

- [ ] Read-only mode and per-user control policy.
- [ ] Multiple monitor layout and client-requested scaling.
- [ ] Clipboard, audio and optional file-transfer channels, each disabled by default.
- [ ] Certificate import, rotation, fingerprint display and audit logs.
- [ ] Connection metrics, frame pacing and adaptive quality.

## Phase 4 — Linux and macOS

- [ ] Linux X11 capture/input backend.
- [ ] Linux Wayland portal and PipeWire backend with visible user authorization.
- [ ] macOS ScreenCaptureKit and Accessibility backend.
- [ ] Per-platform authentication providers while keeping the shared authorization model.
- [ ] CI builds for Windows, Linux X11, Linux Wayland and macOS.

New features should be added as capabilities behind explicit configuration switches. The RDP core should receive typed display/input/channel interfaces rather than platform-specific conditionals.
