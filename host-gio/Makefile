# Laps Host Makefile

.PHONY: all build clean app-bundle install-symlink

BINARY_NAME=laps
CLI_BINARY=laps-cli
BIN_DIR=build/bin
CMD_DIR=cmd/laps
BUILD_DIR=build
PACKAGING_DIR=packaging

all: build install-symlink

build:
	@echo "🔨 Building $(BINARY_NAME)..."
	@mkdir -p $(BIN_DIR)
	go build -o $(BIN_DIR)/$(BINARY_NAME) ./$(CMD_DIR)
	@echo "✅ Build complete: $(BIN_DIR)/$(BINARY_NAME)"

install-symlink: build
	@echo "🔗 Creating $(CLI_BINARY) symlink..."
	@ln -sf $(BINARY_NAME) $(BIN_DIR)/$(CLI_BINARY)
	@echo "✅ Symlink created: $(BIN_DIR)/$(CLI_BINARY) -> $(BINARY_NAME)"

app-bundle: build
	@echo "📦 Building macOS .app bundle..."
	@mkdir -p $(BUILD_DIR)/Laps.app/Contents/MacOS
	@mkdir -p $(BUILD_DIR)/Laps.app/Contents/Resources
	@cp $(BIN_DIR)/$(BINARY_NAME) $(BUILD_DIR)/Laps.app/Contents/MacOS/
	@cp $(PACKAGING_DIR)/darwin/Info.plist $(BUILD_DIR)/Laps.app/Contents/
	@if [ -f $(PACKAGING_DIR)/darwin/icon.icns ]; then \
		cp $(PACKAGING_DIR)/darwin/icon.icns $(BUILD_DIR)/Laps.app/Contents/Resources/; \
		echo "🎨 Icon copied"; \
	else \
		echo "⚠️  No icon.icns found, using default. Place one at $(PACKAGING_DIR)/darwin/icon.icns"; \
	fi
	@echo "🔏 Ad-hoc signing..."
	@codesign --sign - --force --deep $(BUILD_DIR)/Laps.app
	@echo "✅ App bundle ready: $(BUILD_DIR)/Laps.app"

clean:
	@echo "🧹 Cleaning..."
	@rm -rf $(BUILD_DIR)
	@go clean
	@echo "✅ Clean complete"
