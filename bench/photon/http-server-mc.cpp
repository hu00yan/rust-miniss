/*
Photon HTTP perf server, multi-vCPU variant for the rust-miniss comparison.

Base: PhotonLibOS examples/perf/http/http-server.cpp (Apache-2.0, Alibaba).
This file is THEIR benchmark with two additive knobs, following the
multi-vCPU pattern of THEIR examples/perf/net-perf.cpp (echo_server):
  --vcpus N          : N OS threads, each with its own photon::init() and its
                       own SO_REUSEPORT TCP listener bound to the SAME port
                       (kernel-dispatch model, mirrors miniss --cores N).
                       vcpus=1 keeps the official single-vCPU path.
  --event_engine     : default (photon::INIT_EVENT_DEFAULT -> epoll on Linux,
                       see photon.cpp recommended_order) | epoll | iouring
                       (INIT_EVENT_IOURING; the net-perf.cpp "iouring" mode).
  --socket           : tcp (official http-server uses new_tcp_socket_server)
                       | iouring (new_iouring_tcp_server, net-perf's io_uring
                       socket type).
The request handler, response body construction, gflags and logging are
byte-identical to the upstream file, except qps is atomic because multiple
vCPUs increment it.
*/

#include <sys/fcntl.h>
#include <netinet/tcp.h>
#include <chrono>
#include <thread>
#include <atomic>
#include <vector>
#include <gflags/gflags.h>
#include <photon/thread/thread11.h>
#include <photon/io/signal.h>
#include <photon/fs/localfs.h>
#include <photon/common/alog-stdstring.h>
#include <photon/io/fd-events.h>
#include <photon/net/socket.h>
#include <photon/net/http/server.h>

using namespace photon;

DEFINE_int32(port, 19876, "port");
DEFINE_int32(body_size, 4096, "http body size");
DEFINE_bool(serve_file, false, "serve static file");
DEFINE_int32(vcpus, 1, "number of photon vCPUs (one OS thread each, SO_REUSEPORT on one port)");
DEFINE_string(event_engine, "default", "default|epoll|iouring");
DEFINE_string(socket, "tcp", "tcp|iouring");

static bool stop_flag = false;
static std::string data_str;
static std::atomic<uint64_t> qps{0};
static const char* file_name = "static-file.test";

static void stop_handler(int signal) { stop_flag = true; }

static void show_qps_loop() {
    while (!stop_flag) {
        photon::thread_sleep(1);
        LOG_INFO("qps: `", qps.load());
        qps = 0;
    }
}

class SimpleHandler : public net::http::HTTPHandler {
public:
    int handle_request(net::http::Request& req, net::http::Response& resp, std::string_view) {
        auto target = req.target();
        resp.set_result(200);
        resp.headers.content_length((size_t) FLAGS_body_size);
        resp.headers.insert("Server", "nginx/1.14.1");
        resp.headers.insert("Content-Type", "application/octet-stream");
        auto ret_w = resp.write((void*) data_str.data(), FLAGS_body_size);
        if (ret_w != (ssize_t) FLAGS_body_size) {
            LOG_ERRNO_RETURN(0, -1,
                             "send body failed, target: `, `", target, VALUE(ret_w));
        }
        qps++;
        return 0;
    }
};

static int select_event_engine() {
    if (FLAGS_event_engine == "epoll") return photon::INIT_EVENT_EPOLL;
    if (FLAGS_event_engine == "iouring") return photon::INIT_EVENT_IOURING;
    return photon::INIT_EVENT_DEFAULT;
}

static void serve_on_vcpu(int port) {
    // Runs on a thread that already called photon::init().
    auto tcpserv = (FLAGS_socket == "iouring")
        ? net::new_iouring_tcp_server()
        : net::new_tcp_socket_server();
    if (!tcpserv) LOG_ERROR_RETURN(0, , "fail to create server");
    DEFER(delete tcpserv);
    tcpserv->setsockopt<int>(SOL_SOCKET, SO_REUSEPORT, 1);
    tcpserv->bind_v4any(port);
    tcpserv->listen();
    auto http_srv = net::http::new_http_server();
    DEFER(delete http_srv);
    SimpleHandler handler;
    http_srv->add_handler(&handler);
    tcpserv->set_handler(http_srv->get_connection_handler());
    tcpserv->start_loop();
    while (!stop_flag) {
        photon::thread_sleep(1);
    }
}

int main(int argc, char** argv) {
    gflags::ParseCommandLineFlags(&argc, &argv, true);
    set_log_output_level(ALOG_INFO);
    int engine = select_event_engine();
    if (photon::init(engine, photon::INIT_IO_NONE))
        return -1;
    DEFER(photon::fini());

    photon::block_all_signal();
    photon::sync_signal(SIGINT, &stop_handler);
    photon::sync_signal(SIGTERM, &stop_handler);
    photon::sync_signal(SIGTSTP, &stop_handler);
    data_str.resize(FLAGS_body_size);
    for (auto& c : data_str) c = '0';

    thread_create11(show_qps_loop);

    int n = FLAGS_vcpus < 1 ? 1 : FLAGS_vcpus;
    std::vector<std::thread> extra;
    for (int i = 1; i < n; ++i) {
        extra.emplace_back([port = FLAGS_port, engine]() {
            if (photon::init(engine, photon::INIT_IO_NONE)) {
                LOG_ERROR_RETURN(0, , "vcpu photon::init failed");
            }
            DEFER(photon::fini());
            serve_on_vcpu(port);
        });
    }
    serve_on_vcpu(FLAGS_port);
    for (auto& t : extra) t.join();
    LOG_INFO("test stopped");
}
