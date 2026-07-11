#!/usr/bin/env sh
set -eu

if [ "$(uname -s)" != "Linux" ]; then
    echo "linux-safety requires Linux kernel 6.12+ with io_uring" >&2
    exit 1
fi

echo "PARTIALLY VERIFIED Linux/io_uring obligations:" >&2
echo "- delayed-submit readv/writev pointer stability: covered below" >&2
echo "- descriptor lease across numeric fd reuse: covered by runtime regressions" >&2
echo "UNVERIFIED Linux/io_uring obligations:" >&2
echo "- SQ pressure and io_uring_enter submission-failure injection" >&2
echo "- cancellation success and terminal CQE ordering across kernel variants" >&2
echo "- multishot accept overflow/rearm and shutdown drain under sustained load" >&2
echo "- termination snapshot proving zero tasks, timers, messages, operations, and retained descriptors" >&2

cargo test --test runtime_regressions tcp_read_rent_reads_into_empty_capacity_buffer -- --exact
cargo test --test runtime_regressions tcp_accept_multishot_completes_after_delayed_connect -- --exact
cargo test --test test_fast_poll fast_poll_tests::test_poll_add_oneshot -- --exact
cargo test --test vectored_io linux_uring_delayed_submit_readv_survives_relocation_pressure -- --exact
cargo test --test vectored_io linux_uring_delayed_submit_writev_survives_relocation_pressure -- --exact
