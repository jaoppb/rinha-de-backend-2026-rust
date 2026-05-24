// spike_recovery.js — Spike intencional + medição de tempo de recuperação.
//
// Objetivo: sobrecarregar o sistema até ele começar a degradar e então medir
// quanto tempo leva para o p99 voltar ao baseline após a carga cair.
//
// Se o p99 não voltar a <20ms nos 60s de recovery, há state acumulado:
//   - GC Go com backlog de objetos não coletados (GOGC alto demais?)
//   - epoll backlog não drena (fds pendentes no queue)
//   - mmap pages evicted pelo OS durante o pico (page faults no recovery)
//
// Stages:
//   0-30s:  baseline 500 req/s (warm)
//   30-45s: ramping rápido → 3000 req/s (sobrecarga intencional)
//   45-75s: sustenta 3000 req/s (sistema sob pressão máxima)
//   75-80s: queda brusca → 200 req/s
//   80-140s: recovery — o p99 volta ao baseline? Em quanto tempo?
//
// Execute: k6 run test/spike_recovery.js
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
	summaryTrendStats: ['p(50)', 'p(95)', 'p(99)', 'p(99.9)', 'max'],
	systemTags: ['status', 'method'],
	dns: { ttl: '5m', select: 'roundRobin' },
	scenarios: {
		spike: {
			executor: 'ramping-arrival-rate',
			startRate: 500,
			timeUnit: '1s',
			preAllocatedVUs: 300,
			maxVUs: 6000,
			gracefulStop: '15s',
			stages: [
				{ duration: '30s', target: 500  }, // Baseline aquecido
				{ duration: '15s', target: 3000 }, // Spike rápido (sobrecarga)
				{ duration: '30s', target: 3000 }, // Sustenta o spike
				{ duration: '5s',  target: 200  }, // Queda brusca
				{ duration: '60s', target: 200  }, // Recovery — p99 volta?
			],
		},
	},
};

export function setup() {
	console.log(
		`[spike_recovery] Dataset: ${expectedStats.total} entries\n` +
		`Stages: 30s baseline → 15s spike 3000/s → 30s sustain → 5s queda → 60s recovery`,
	);
	console.log('Meta: p99 < 20ms nos últimos 30s de recovery');
}

export default function () {
	const idx   = exec.scenario.iterationInTest % testData.length;
	const entry = testData[idx];
	const expectedApproved = entry.expected_approved;

	const res = http.post(
		'http://localhost:9999/fraud-score',
		JSON.stringify(entry.request),
		{ headers: { 'Content-Type': 'application/json' }, timeout: '2001ms' },
	);

	const elapsed = Date.now() - exec.scenario.startTime;
	if (elapsed > 80000 && res.timings.duration > 10) {
		console.warn(`[RECOVERY ALERT] Latency still high during recovery: ${res.timings.duration}ms at ${Math.round(elapsed/1000)}s`);
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
        scenario: 'spike_recovery',
        expected: expectedStats,
        p99: r(p99, PRECISION) + 'ms',
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
        'test_results/spike.json': JSON.stringify(result, null, 2),
        stdout: textSummary(data, { indent: ' ', enableColors: true }),
    };
}
