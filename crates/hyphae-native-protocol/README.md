# Hyphae Native protocol

Portable `HYPHLCL1` frame, handshake, product request/response, `HYPERR01`,
stream completion, flow-control, cancellation, and deadline codecs.

This crate is a transport boundary. It delegates frame compatibility to the
current native runtime codec and carries only product-owned operations. It does
not implement data-engine behavior or engine-to-engine communication.
