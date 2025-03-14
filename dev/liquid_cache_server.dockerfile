# Use a minimal Debian base image
FROM ubuntu:24.04

# Install minimal runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ENV RUST_BACKTRACE=1
ENV RUST_LOG=info

WORKDIR /app

COPY ./target/release/bench_server /app/bench_server

EXPOSE 50051
EXPOSE 50052

# Run the server when the container starts
CMD ["/app/bench_server", "--address", "0.0.0.0:50051", "--admin-address", "0.0.0.0:50052"]
