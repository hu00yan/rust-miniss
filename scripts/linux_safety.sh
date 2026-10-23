#!/usr/bin/env sh
set -eu

if [ "$(uname -s)" != "Linux" ]; then
    echo "linux-safety requires Linux kernel 6.12+ with io_uring" >&2
    exit 1
fi

kernel=$(uname -r)
major=$(printf '%s\n' "$kernel" | cut -d. -f1)
minor=$(printf '%s\n' "$kernel" | cut -d. -f2)
case "$major:$minor" in
    *[!0-9:]*|:|*:)
        echo "SKIP: cannot parse Linux kernel version for io_uring safety: $kernel"
        exit 0
        ;;
esac
if [ "$major" -lt 6 ] || { [ "$major" -eq 6 ] && [ "$minor" -lt 12 ]; }; then
    echo "SKIP: Linux $kernel selects the mio/epoll compatibility backend; io_uring safety requires kernel 6.12+"
    exit 0
fi
if [ "${MINISS_IO_BACKEND:-}" = "epoll" ]; then
    echo "SKIP: MINISS_IO_BACKEND=epoll selects the compatibility backend; io_uring safety is not applicable"
    exit 0
fi

echo "PARTIALLY VERIFIED Linux/io_uring obligations:" >&2
echo "- delayed-submit readv/writev pointer stability: covered below" >&2
echo "- descriptor lease across numeric fd reuse: covered by runtime regressions" >&2
echo "- queued read cancellation and terminal CQE: covered by io::uring unit test" >&2
echo "- SQ-full flush/retry accounting: covered by io::uring unit test" >&2
echo "- bounded multishot accept overflow closes excess fd: covered by io::uring unit test" >&2
echo "- pending multishot accept shutdown drain: covered below" >&2
echo "- multishot accept overflow descriptor drain: repeated bursts covered below" >&2
echo "- 4-worker loaded timer shutdown termination snapshot: covered below" >&2
echo "- parked-worker cross-core eventfd wake: covered below" >&2
echo "- 256-task cross-core bounded admission and affinity: covered below" >&2
echo "- io_uring_enter EBADF injection and queued-SQE retry: covered below" >&2
echo "- two-worker QUIC endpoint ownership: covered below" >&2
echo "- QUIC peer-initiated close propagation: covered below" >&2
echo "- two-worker UDP socket ownership and close cleanup: covered below" >&2
echo "UNVERIFIED Linux/io_uring obligations:" >&2
echo "- multishot accept overflow under alternate CQ drain batch shapes" >&2
echo "- sustained multi-core loaded termination snapshot matrix" >&2

run_exact() {
    ./scripts/run_exact_test.sh cargo test "$@"
}

run_exact --test runtime_regressions tcp_read_rent_reads_into_empty_capacity_buffer -- --exact
run_exact --test runtime_regressions tcp_accept_multishot_completes_after_delayed_connect -- --exact
run_exact --test runtime_regressions tcp_accept_multishot_rearms_for_a_bounded_burst -- --exact
run_exact --test runtime_regressions tcp_multishot_accept_shutdown_drains_pending_listener -- --exact
run_exact --test runtime_regressions tcp_multishot_accept_reclaims_multiple_overflow_bursts_on_listener_drop -- --exact
run_exact --test runtime_regressions tcp_pending_read_drop_is_drained_before_late_data_arrives -- --exact
run_exact --lib io::uring::tests::queued_read_cancel_drains_terminal_cqe -- --exact
run_exact --lib io::uring::tests::submitted_read_cancel_drains_terminal_cqe -- --exact
run_exact --lib io::uring::tests::small_completion_drain_batches_reclaim_all_terminal_operations -- --exact
run_exact --lib multicore::tests::termination_snapshot_is_zero_after_join -- --exact
run_exact --lib multicore::tests::cross_core_spawn_wakes_a_parked_worker -- --exact
run_exact --lib multicore::tests::cross_core_spawn_stress_keeps_bounded_admission_and_affinity -- --exact
run_exact --test runtime_regressions multicore_loaded_timer_shutdown_publishes_zero_termination_snapshot -- --exact
run_exact --all-features --test quinn_h3 quinn_endpoints_run_on_separate_owned_shards -- --exact
run_exact --all-features --test quinn_h3 quinn_peer_close_is_observed_by_the_client -- --exact
run_exact --all-features --test integration_http test_http_incomplete_body_eof_is_rejected_and_connection_finishes -- --exact
run_exact --all-features --test integration_http test_http_transfer_encoding_is_rejected_and_connection_finishes -- --exact
for round in 1 2 3; do
    MINISS_QUIC_CONNECTIONS=512 run_exact --all-features --test quinn_h3 quinn_endpoints_run_on_separate_owned_shards -- --exact
done
run_exact --all-features --test integration_multicore multicore_sustained_cross_core_wake_pressure_drains_cleanly -- --exact
run_exact --all-features --test integration_multicore multicore_udp_owned_shards_exchange_and_close_cleanly -- --exact
for round in 1 2 3; do
    MINISS_UDP_CLOSE_RACE_SOCKETS=64 run_exact --all-features --test stress_test test_multicore_udp_receive_close_race_reclaims_state -- --exact
done
run_exact --lib io::uring::tests::io_uring_enter_failure_preserves_queued_submission_for_retry -- --exact
run_exact --lib io::uring::tests::sq_full_retry_preserves_pending_submission_count -- --exact
run_exact --test test_fast_poll fast_poll_tests::test_poll_add_oneshot -- --exact
run_exact --test test_fast_poll fast_poll_tests::test_poll_add_multishot_completion_and_cancel -- --exact
run_exact --test vectored_io linux_uring_delayed_submit_readv_survives_relocation_pressure -- --exact
run_exact --test vectored_io linux_uring_delayed_submit_writev_survives_relocation_pressure -- --exact
