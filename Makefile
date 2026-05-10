.PHONY: build build-release up down restart logs smoke test docker-push

DOCKER_COMPOSE = docker-compose
K6_IMAGE = grafana/k6
PWD = $(shell pwd)
IMAGE_NAME = jaoppb/rinha-2026-rust:latest

# Default dev build: debug profile + example data
build:
	$(DOCKER_COMPOSE) build --build-arg INPUT_FILE=resources/example-references.json

# Release build: release profile + full data
build-release:
	docker build -t rinha-de-backend-2026-api1 .
	$(DOCKER_COMPOSE) build --build-arg INPUT_FILE=resources/references.json.gz data-loader

up:
	$(DOCKER_COMPOSE) up -d

down:
	$(DOCKER_COMPOSE) down

restart:
	$(DOCKER_COMPOSE) down && make build && $(DOCKER_COMPOSE) up -d

logs:
	$(DOCKER_COMPOSE) logs -f

smoke:
	docker run --rm --network host -i $(K6_IMAGE) run - <test/smoke.js

test:
	docker run --rm --network host -v "$(PWD)/test:/test" -i $(K6_IMAGE) run /test/test.js

docker-push:
	docker build -t $(IMAGE_NAME) --build-arg INPUT_FILE=resources/references.json.gz .
	docker push $(IMAGE_NAME)

run-all: restart
	sleep 5
	make smoke
