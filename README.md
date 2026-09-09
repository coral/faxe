# Faxe

Cross platform, open-source native desktop app to send/recieve faxes over SIP written in Rust.

## Debugging a failed fax

Quit the existing desktop first, then capture diagnostics with:

```sh
RUST_LOG=faxe=trace,udptl=trace cargo run 2>&1 | tee /tmp/faxe-debug.log
```

Retry the failed job explicitly from Queue & history. The log includes SIP
requests/responses and SDP, PJSIP transaction details, SpanDSP T.30/T.38 flow,
RTP/UDPTL packet metadata and the final failure reason. Authentication headers
and native hex byte dumps are redacted; phone numbers, SIP identities, station
IDs and network addresses remain, so review logs before sharing them. Redirected
desktop/CLI logs omit ANSI color codes.

For less output, use `RUST_LOG=faxe=debug,faxe_native::sip::wire=trace`.
Individual targets are `faxe_native::pjsip`, `faxe_native::spandsp`,
`faxe_native::sip::wire` and `faxe_native::network`. Trace packet logging is
verbose and can affect timing; enable it only while diagnosing a problem.

## Development dependencies

Packaging configurations for macOS 15+ arm64, Windows Store x64/arm64, Linux
AppImage x64/arm64 and Flatpak are in [packaging/README.md](packaging/README.md).
GitHub Actions builds and packages these targets. Builds on `master` also sign
and notarize macOS downloads using the `release` environment. Microsoft Store
identity and submission remain separate setup steps.

FAXE uses published `spandsp` / `spandsp-sys` 0.2.3 and `udptl` 0.2.0, without
local Cargo overrides. No sibling checkout is needed. The separately published
recovery contract and the audit of the former overrides are recorded in
[spandsp-changes.md](spandsp-changes.md).

## License

FAXE is licensed under GPL-3.0-only. See [LICENSE](LICENSE) and the generated
[acknowledgments](https://faxe.oblique.media/licenses). Dependency licenses remain
separate; [licenses/README.md](licenses/README.md) explains regeneration and
release source obligations. SpanDSP uses only the fax feature; its optional
GPLv2-only V.150 modem-relay modules are excluded.