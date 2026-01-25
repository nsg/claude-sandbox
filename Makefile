.PHONY: shell claude build ensure-running clean install uninstall skills dist

CONTAINER_NAME ?= claude
IMAGE = localhost/claude:latest
WORKSPACE_DIR = $(PWD)
CLAUDE_DIR = $(HOME)/.claude

build:
	podman build \
		--build-arg GIT_USER_NAME="$$(git config user.name)" \
		--build-arg GIT_USER_EMAIL="$$(git config user.email)" \
		-t $(IMAGE) .

ensure-running:
	@podman container exists $(CONTAINER_NAME) || \
		(podman image exists $(IMAGE) || $(MAKE) build && \
		podman create --name $(CONTAINER_NAME) \
			--hostname $(CONTAINER_NAME) \
			-v $(WORKSPACE_DIR):/workspace \
			-v $(CLAUDE_DIR):/root/.claude \
			-e CLAUDE_CONFIG_DIR=/root/.claude \
			-e TERM=xterm-256color \
			-e COLORTERM=truecolor \
			-v /etc/localtime:/etc/localtime:ro \
			-v /etc/timezone:/etc/timezone:ro \
			-p 3456:3456 \
			-it $(IMAGE) /bin/bash)
	@[ "$$(podman inspect -f '{{.State.Running}}' $(CONTAINER_NAME))" = "true" ] || \
		podman start $(CONTAINER_NAME)

shell: ensure-running
	podman exec -w /workspace -it $(CONTAINER_NAME) bash -l

claude: ensure-running
	podman exec -w /workspace -it $(CONTAINER_NAME) bash -lc claude

clean:
	-podman rm -f $(CONTAINER_NAME)
	-podman rmi $(IMAGE)

install:
	@cd claude-sandbox && cargo build --release
	@mkdir -p $(HOME)/bin
	@cp claude-sandbox/target/release/claude-sandbox $(HOME)/bin/claude-sandbox
	@echo "Installed claude-sandbox to $(HOME)/bin/claude-sandbox"
	@echo "Make sure $(HOME)/bin is in your PATH"

uninstall:
	@rm -f $(HOME)/bin/claude-sandbox
	@echo "Removed claude-sandbox from $(HOME)/bin"

skills:
	@mkdir -p $(CLAUDE_DIR)/skills
	@cp -r skills/* $(CLAUDE_DIR)/skills/
	@echo "Installed skills to $(CLAUDE_DIR)/skills/"

dist:
	@mkdir -p dist
	@cd skills && zip -r ../dist/skills.zip .
	@cd skills && tar -czf ../dist/skills.tar.gz .
	@echo "Created dist/skills.zip and dist/skills.tar.gz"
