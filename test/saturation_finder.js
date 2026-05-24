// saturation_finder.js — Escalonamento granular para encontrar o ponto de saturação.
//
// Objetivo: descobrir em qual req/s o p99 ultrapassa 10ms ou o failure_rate
// ultrapassa 0.5%. Cada stage tem 60s de estabilização antes de subir.
//
// O heavyTests.js atual vai de 0→4000 em 10s — rápido demais para medir
// o ponto de saturação com precisão. Aqui cada patamar tem 60s de steady state.
//
// Execute: k6 run test/saturation_finder.js
// NÃO modifica test.js (proibido pela organização).

import http from 'k6/http';
import { SharedArray } from 'k6/data';
import { Counter } from 'k6/metrics';
import { textSummary } from './k6-summary.js';
import exec from 'k6/execution';

const testData = new SharedArray('test-data', function () {
	return JSON.parse(open('./test-data.json')).entries;
});
const statsArr = new SharedArray('test-stats', function () {
	return [JSON.parse(open('./test-data.json')).stats];
});
const expectedStats = statsArr[0];

const tpCount    = new Counter('tp_count');
const tnCount    = new Counter('tn_count');
const fpCount    = new Counter('fp_count');
const fnCount    = new Counter('fn_count');
const errorCount = new Counter('error_count');

export const options = {
	// Estatísticas granulares para identificar o ponto exato de saturação
	summaryTrendStats: ['p(50)', 'p(95)', 'p(99)', 'p(99.9)', 'max', 'avg'],
	systemTags: ['status', 'method'],
	dns: { ttl: '5m', select: 'roundRobin' },
	scenarios: {
		// Escalonamento em 7 stages, cada um com 60s de steady state.
		// Total: ~7 minutos. Rate máximo: 2000 req/s.
		// Ajuste maxVUs se o host não tiver CPU suficiente para tantas goroutines k6.
		ramp: {
			executor: 'ramping-arrival-rate',
			startRate: 200,
			timeUnit: '1s',
			preAllocatedVUs: 200,
			maxVUs: 4000,
			stages: [
				{ duration: '60s', target: 400  }, // Stage 1 — 400 req/s
				{ duration: '60s', target: 600  }, // Stage 2 — 600 req/s
				{ duration: '60s', target: 800  }, // Stage 3 — 800 req/s
				{ duration: '60s', target: 1000 }, // Stage 4 — 1000 req/s
				{ duration: '60s', target: 1200 }, // Stage 5 — 1200 req/s
				{ duration: '60s', target: 1500 }, // Stage 6 — 1500 req/s
				{ duration: '60s', target: 2000 }, // Stage 7 — 2000 req/s (stress)
			],
		},
	},
};

export function setup() {
	console.log(
		`[saturation_finder] Dataset: ${expectedStats.total} entries — ` +
		`ramping 200→2000 req/s in 7 stages of 60s each`,
	);
	console.log('Monitore o p99 por stage — saturação = p99 > 10ms ou failure_rate > 0.5%');
}

export default function () {
	const idx = exec.scenario.iterationInTest % testData.length;
	const entry = testData[idx];
	const expectedApproved = entry.expected_approved;

	const res = http.post(
		'http://localhost:9999/fraud-score',
		JSON.stringify(entry.request),
		{ headers: { 'Content-Type': 'application/json' }, timeout: '2001ms' },
	);

	if (res.timings.duration > 10) {
		console.warn(`[LATENCY ALERT] High latency detected at iteration ${exec.scenario.iterationInTest}. Duration: ${res.timings.duration}ms`);
	}

	if (res.status === 200) {
		const body = JSON.parse(res.body);
		if (expectedApproved === body.approved) {
			if (body.approved) tnCount.add(1);
			else               tpCount.add(1);
		} else {
			if (body.approved) fnCount.add(1);
			else               fpCount.add(1);
		}
	} else {
		errorCount.add(1);
	}
}

export function handleSummary(data) {
    const K = 1000;
    const T_MAX_MS = 1000;
    const P99_MIN_MS = 1;
    const P99_MAX_MS = 2000;
    const EPSILON_MIN = 0.001;
    const BETA = 300;
    const TX_CORTE = 0.15;
    const SCORE_P99_CORTE = -3000;
    const SCORE_DET_CORTE = -3000;
    const PRECISION = __ENV.SCORE_PRECISION ? parseInt(__ENV.SCORE_PRECISION) : 2;

    const r = (v, decimals) => +v.toFixed(decimals);

    const httpDuration = data.metrics.http_req_duration.values;
    const p99 = httpDuration['p(99)'];

    const tp = data.metrics.tp_count ? data.metrics.tp_count.values.count : 0;
    const tn = data.metrics.tn_count ? data.metrics.tn_count.values.count : 0;
    const fp = data.metrics.fp_count ? data.metrics.fp_count.values.count : 0;
    const fn = data.metrics.fn_count ? data.metrics.fn_count.values.count : 0;
    const errs = data.metrics.error_count ? data.metrics.error_count.values.count : 0;

    const N = tp + tn + fp + fn + errs;

    // Erros ponderados (para a fórmula log) e contagem pura (para o corte)
    const E = (fp * 1) + (fn * 3) + (errs * 5);
    const failures = fp + fn + errs;
    const epsilon = N > 0 ? E / N : 0;
    const failureRate = N > 0 ? failures / N : 0;

    // Score P99 (log, com teto em P99_MIN_MS e corte em P99_MAX_MS).
    // p99=0 = nenhuma resposta completou; retorna 0 pra evitar Infinity no JSON.
    let p99Score;
    let p99CutTriggered = false;
    if (p99 <= 0) {
        p99Score = 0;
    } else if (p99 > P99_MAX_MS) {
        p99Score = SCORE_P99_CORTE;
        p99CutTriggered = true;
    } else {
        p99Score = K * Math.log10(T_MAX_MS / Math.max(p99, P99_MIN_MS));
    }

    // Score detecção (log com penalidade absoluta, ou corte em -3000 se falhas > 15%)
    let detScore;
    let rateComponent = 0;
    let absolutePenalty = 0;
    let cutTriggered = false;
    if (failureRate > TX_CORTE) {
        detScore = SCORE_DET_CORTE;
        cutTriggered = true;
    } else {
        rateComponent = K * Math.log10(1 / Math.max(epsilon, EPSILON_MIN));
        absolutePenalty = -BETA * Math.log10(1 + E);
        detScore = rateComponent + absolutePenalty;
    }

    const finalScore = p99Score + detScore;

    const result = {
        scenario: 'saturation_finder',
        expected: expectedStats,
        p99: r(p99, PRECISION) + 'ms',
        diagnostics: {
            http_req_waiting: {
                p95: r(reqWaiting['p(95)'] || 0, PRECISION),
                p99: r(reqWaiting['p(99)'] || 0, PRECISION),
                p999: r(reqWaiting['p(99.9)'] || 0, PRECISION),
            },
            http_req_connecting: {
                p95: r(reqConnecting['p(95)'] || 0, PRECISION),
                p99: r(reqConnecting['p(99)'] || 0, PRECISION),
                p999: r(reqConnecting['p(99.9)'] || 0, PRECISION),
            },
            http_req_tls_handshaking: {
                p95: r(reqTlsHandshaking['p(95)'] || 0, PRECISION),
                p99: r(reqTlsHandshaking['p(99)'] || 0, PRECISION),
                p999: r(reqTlsHandshaking['p(99.9)'] || 0, PRECISION),
            }
        },
        scoring: {
            breakdown: {
                false_positive_detections: fp,
                false_negative_detections: fn,
                true_positive_detections: tp,
                true_negative_detections: tn,
                http_errors: errs,
            },
            failure_rate: r(failureRate * 100, PRECISION) + '%',
            weighted_errors_E: E,
            error_rate_epsilon: r(epsilon, PRECISION + 4),
            p99_score: {
                value: r(p99Score, PRECISION),
                cut_triggered: p99CutTriggered,
            },
            detection_score: {
                value: r(detScore, PRECISION),
                rate_component: cutTriggered ? null : r(rateComponent, PRECISION),
                absolute_penalty: cutTriggered ? null : r(absolutePenalty, PRECISION),
                cut_triggered: cutTriggered,
            },
            final_score: r(finalScore, PRECISION),
            raw: {
                p99_ms: p99,
                failure_rate: failureRate,
                error_rate_epsilon: epsilon,
                p99_score: p99Score,
                detection_score: detScore,
                rate_component: cutTriggered ? null : rateComponent,
                absolute_penalty: cutTriggered ? null : absolutePenalty,
                final_score: finalScore,
            },
        },
    };

    return {
        'test_results/saturation.json': JSON.stringify(result, null, 2),
        stdout: textSummary(data, { indent: ' ', enableColors: true }),
    };
}
