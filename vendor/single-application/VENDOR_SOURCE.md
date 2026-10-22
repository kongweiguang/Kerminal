<!-- @author kongweiguang -->

# Vendored source

This directory starts from the published `single-application` 0.1.1 crate.

- Upstream repository: <https://github.com/mengyou1024/single-application>
- Upstream source revision recorded by the package: `f38a3f11e2cc56addca76e70d580dace0b974ec1`
- Registry package checksum from the frozen workspace lockfile: `17746899b5bc1faf0d4bad99809db8e711db4b427b381a22d915b2f4c8b05d03`
- Package license: MIT; the upstream `LICENSE` file is retained unchanged.

The local patch is limited to the Tokio primary-listener lifecycle. It retains
the existing JSON message framing and socket name, bounds concurrent accepted
connections to the existing channel capacity and each frame to 128 KiB, and
gives the listener task a cancellable, deadline-aware join owner. Dropping the
primary handle requests listener shutdown; normal desktop shutdown still keeps
and joins the explicit owner. An additive `try_send_message` reports transport
errors while the original `send_message` remains available for compatibility.
The wire protocol and message shape are unchanged.
