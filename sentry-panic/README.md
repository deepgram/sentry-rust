# sentry-panic

The Sentry Panic handler Integration.

The `PanicIntegration`, which is enabled by default in `sentry`, installs a
panic handler that will automatically dispatch all errors to Sentry that
are caused by a panic.
Additionally, panics are forwarded to the previously registered panic hook.

## Configuration

The panic integration can be configured with an additional extractor, which
might optionally create a sentry `Event` out of a `PanicInfo`.

```rust
let integration = sentry_panic::PanicIntegration::default().add_extractor(|info| None);
```

License: Apache-2.0
