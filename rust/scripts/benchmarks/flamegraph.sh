#!/bin/bash
# Profiling script for jetstream-turbo
# Generates CPU flamegraphs to identify performance bottlenecks

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
OUTPUT_DIR="$PROJECT_DIR/flamegraphs"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}=== Jetstream Turbo CPU Profiling ===${NC}"
echo ""

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Check if cargo-flamegraph is installed
if ! command -v cargo-flamegraph &> /dev/null; then
    echo -e "${RED}Error: cargo-flamegraph not installed${NC}"
    echo "Install with: cargo install flamegraph"
    exit 1
fi

# Default values
DURATION=60
TITLE="jetstream-turbo"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --duration)
            DURATION="$2"
            shift 2
            ;;
        --title)
            TITLE="$2"
            shift 2
            ;;
        --help)
            echo "Usage: $0 [options]"
            echo ""
            echo "Options:"
            echo "  --duration SECONDS  Duration to profile (default: 60)"
            echo "  --title TITLE      Title for the flamegraph (default: jetstream-turbo)"
            echo ""
            echo "Examples:"
            echo "  $0                      # Profile for 60 seconds"
            echo "  $0 --duration 30        # Profile for 30 seconds"
            echo "  $0 --title my-profile   # Custom title"
            exit 0
            ;;
        *)
            echo -e "${RED}Unknown option: $1${NC}"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

OUTPUT_FILE="$OUTPUT_DIR/${TITLE}_$(date +%Y%m%d_%H%M%S).svg"

echo -e "${YELLOW}Configuration:${NC}"
echo "  Duration: ${DURATION}s"
echo "  Title:    $TITLE"
echo "  Output:   $OUTPUT_FILE"
echo ""

# Check for .env file
if [ ! -f "$PROJECT_DIR/.env" ]; then
    echo -e "${YELLOW}Warning: .env file not found${NC}"
    echo "Some features may not work without configuration"
fi

echo -e "${GREEN}Starting flamegraph generation...${NC}"
echo -e "${YELLOW}Press Ctrl+C to stop early${NC}"
echo ""

# Run the profiler
# Note: On macOS, you may need to disable SIP or use sudo for full profiling
# cargo-flamegraph uses dtrace which requires special permissions on macOS

cd "$PROJECT_DIR/rust"

if command -v sudo &> /dev/null && [ "$(uname)" = "Darwin" ]; then
    # Try with sudo on macOS
    echo "Running on macOS - attempting with elevated privileges..."
    sudo -n cargo flamegraph \
        --bin jetstream-turbo \
        --title "$TITLE" \
        --duration "$DURATION" \
        --output "$OUTPUT_FILE" \
        -- \
        --modulo 4 \
        --shard 0 2>&1 || {
        echo ""
        echo -e "${YELLOW}Note: Full profiling requires disabling SIP on macOS${NC}"
        echo "Or run: cargo flamegraph without sudo (limited results)"
        exit 1
    }
else
    cargo flamegraph \
        --bin jetstream-turbo \
        --title "$TITLE" \
        --duration "$DURATION" \
        --output "$OUTPUT_FILE" \
        -- \
        --modulo 4 \
        --shard 0
fi

echo ""
echo -e "${GREEN}Flamegraph saved to: $OUTPUT_FILE${NC}"
echo ""

# Try to open the flamegraph
if command -v open &> /dev/null; then
    echo "Opening flamegraph..."
    open "$OUTPUT_FILE"
elif command -v xdg-open &> /dev/null; then
    echo "Opening flamegraph..."
    xdg-open "$OUTPUT_FILE"
else
    echo "Open $OUTPUT_FILE in a browser to view the flamegraph"
fi
