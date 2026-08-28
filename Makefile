IMAGE       := justgu1/proviz-elekto
IMAGE_AMD64 := justgu1/proviz-elekto-amd64
VERSION     := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')

.PHONY: build build-amd64 push push-amd64 all release

build:
	docker build -t $(IMAGE):latest -t $(IMAGE):v$(VERSION) .

build-amd64:
	docker buildx build --platform linux/amd64 \
		-t $(IMAGE_AMD64):latest -t $(IMAGE_AMD64):v$(VERSION) \
		--load .

push: build
	docker push $(IMAGE):latest
	docker push $(IMAGE):v$(VERSION)

push-amd64: build-amd64
	docker push $(IMAGE_AMD64):latest
	docker push $(IMAGE_AMD64):v$(VERSION)

release: push push-amd64
