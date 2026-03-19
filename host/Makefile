# Laps Host Makefile

.PHONY: all build clean

BINARY_NAME=laps
BIN_DIR=bin
CMD_DIR=cmd/laps

all: build

build:
	@echo "🔨 Building $(BINARY_NAME)..."
	@mkdir -p $(BIN_DIR)
	go build -o $(BIN_DIR)/$(BINARY_NAME) ./$(CMD_DIR)
	@echo "✅ Build complete: $(BIN_DIR)/$(BINARY_NAME)"

clean:
	@echo "🧹 Cleaning..."
	@rm -rf $(BIN_DIR)
	@go clean
	@echo "✅ Clean complete"
