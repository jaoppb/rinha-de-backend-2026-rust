.PHONY: build build-release build-verbose build-release-verbose up down restart logs smoke test test-thermal test-sustained test-saturation test-spike docker-push build-images preview monitor run-all collect-benchmarks

DOCKER_COMPOSE = docker-compose
K6_IMAGE = grafana/k6
PWD = $(shell pwd)
API_IMAGE = jaoppb/rinha-2026-rust:latest
LB_IMAGE = jaoppb/rinha-2026-lb:latest
INPUT_FILE ?= resources/example-references.json

build-images:
	docker build -t $(API_IMAGE) --build-arg INPUT_FILE=$(INPUT_FILE) .
	docker build -t $(LB_IMAGE) lb/

build:
	$(MAKE) build-images INPUT_FILE=resources/example-references.json

build-release:
	$(MAKE) build-images INPUT_FILE=resources/references.json.gz

build-verbose:
	docker build -t $(API_IMAGE) --build-arg INPUT_FILE=resources/example-references.json --build-arg FEATURES="--features verbose-logging" .
	docker build -t $(LB_IMAGE) --build-arg FEATURES="--features verbose-logging" lb/

build-release-verbose:
	LOG_TRANSPORT=json docker build -t $(API_IMAGE) --build-arg INPUT_FILE=resources/references.json.gz --build-arg FEATURES="--features verbose-logging" .
	LOG_TRANSPORT=json docker build -t $(LB_IMAGE) --build-arg FEATURES="--features verbose-logging" lb/

up:
	LOG_TRANSPORT=json $(DOCKER_COMPOSE) up -d

down:
	$(DOCKER_COMPOSE) down

restart: down build up

logs:
	$(DOCKER_COMPOSE) logs -f

smoke:
	docker run --rm --network host -i $(K6_IMAGE) run - <test/smoke.js

docker-push: build-release
	docker push $(API_IMAGE)
	docker push $(LB_IMAGE)

run-all: restart smoke

preview:
	gh issue create --repo zanfranceschi/rinha-de-backend-2026 --title "preview" --body "rinha/test jaoppb-rust"

monitor:
	LOG_TRANSPORT=json ./scripts/monitor.sh

test_results/%.json:
	mkdir -p test_results
	touch $@

smoke: test_results/smoke.json
	docker-compose -f test/docker-compose.yml --profile smoke up

test: test_results/default.json
	docker-compose -f test/docker-compose.yml --profile test up

test-thermal: test_results/thermal.json
	docker-compose -f test/docker-compose.yml --profile thermal up

test-sustained: test_results/sustained.json
	docker-compose -f test/docker-compose.yml --profile sustained up

test-saturation: test_results/saturation.json
	docker-compose -f test/docker-compose.yml --profile saturation up

test-spike: test_results/spike.json
	docker-compose -f test/docker-compose.yml --profile spike up

TEST_TYPE ?= test

collect-benchmarks:
	@echo "==> Running Baseline (Non-Verbose) <=="
	rm -f test_results/*.json
	$(MAKE) build-release
	$(MAKE) down
	$(MAKE) up
	@echo "Waiting for services to be ready..."
	@while ! $(MAKE) smoke > /dev/null 2>&1; do sleep 2; done
	$(MAKE) $(TEST_TYPE)
	mkdir -p test_results/baseline
	cp test_results/*.json test_results/baseline/ 2>/dev/null || true
	
	@echo "==> Running Analysis (Verbose) <=="
	rm -f test_results/*.json
	$(MAKE) build-release-verbose
	$(MAKE) down
	$(MAKE) up
	@echo "Waiting for services to be ready..."
	@while ! $(MAKE) smoke > /dev/null 2>&1; do sleep 2; done
	$(MAKE) $(TEST_TYPE)
	mkdir -p test_results/verbose
	cp test_results/*.json test_results/verbose/ 2>/dev/null || true
	$(DOCKER_COMPOSE) logs lb api1 api2 > test_results/verbose/$(TEST_TYPE)_api.log
	
	@echo "==> Collection Complete <=="

