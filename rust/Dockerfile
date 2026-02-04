# Multi-stage build for optimized production image
FROM rust:1.88 as builder

WORKDIR /app

# Copy dependency files first (better caching)
COPY Cargo.toml Cargo.lock ./
COPY src ./src/

# Build the application in release mode
RUN cargo build --release

# Production stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create app user
RUN useradd --create-home --shell /bin/false app

WORKDIR /app

# Copy the binary from builder stage
COPY --from=builder /app/target/release/jetstream-turbo /usr/local/bin/

# Set ownership and permissions
RUN chown app:app /usr/local/bin/jetstream-turbo

# Switch to non-root user
USER app

# Expose port
EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD wget --no-verbose --tries=1 --spider --quiet --output-document - -O- http://localhost:8080/health || exit 1

# Set default environment variables
ENV RUST_LOG=info
ENV TURBO_BATCH_SIZE=10
ENV TURBO_MAX_CONCURRENT=100

# Run the application
CMD ["jetstream-turbo"]