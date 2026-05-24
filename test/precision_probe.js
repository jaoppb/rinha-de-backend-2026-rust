// precision_probe.js — Sonda de precisão de detecção com carga mínima.
//
// Objetivo: percorrer o dataset COMPLETO em baixa concorrência (4 VUs)
// para medir a taxa de detecção sem ruído de throughput. Útil para:
//
//   1. Verificar se otimizações de nprobe/vectorização mudaram FP/FN
//   2. Identificar qual fração do dataset produz mais erros (por VU offset)
//   3. Medir a latência "base" sem pressão de concorrência
//
// Com 4 VUs e 54.100 entries, cada VU processa ~13.525 entries sequencialmente.
// O tempo esperado: ~54.100 / (4 VUs × throughput_1VU). Em média, cada VU
// faz ~1 req/s com timeout conservador → ~3h. Use maxDuration para cortar.
// Recomendado: ajustar maxDuration para o tempo disponível.
//
// Execute: k6 run test/precision_probe.js
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

// Número de VUs — cada VU percorre uma fatia sequencial do dataset
const NUM_VUS = 4;

export const options = {
	summaryTrendStats: ['p(50)', 'p(95)', 'p(99)', 'max', 'avg'],
	systemTags: ['status', 'method'],
	scenarios: {
		// per-vu-iterations: cada VU faz N iterações sequencialmente.
		// O offset por VU garante que o dataset seja coberto sem sobreposição.
		precision: {
			executor: 'per-vu-iterations',
			vus: NUM_VUS,
			// Divide o dataset entre as VUs (ceiling para cobrir tudo)
			iterations: Math.ceil(testData.length / NUM_VUS),
			maxDuration: '30m', // Corte de segurança; ajuste conforme necessário
		},
	},
};

export function setup() {
	const perVu = Math.ceil(testData.length / NUM_VUS);
	console.log(
		`[precision_probe] Dataset: ${testData.length} entries ÷ ${NUM_VUS} VUs ` +
		`= ${perVu} entries/VU. Carga mínima — sem pressão de throughput.`,
	);
	console.log('Meta: identificar FP/FN sem ruído de concorrência.');
}

export default function () {
	// Cada VU acessa uma fatia não-sobreposta e sequencial do dataset
	const vuIndex     = exec.vu.idInTest % NUM_VUS;      // 0, 1, 2, 3
	const sliceStart  = vuIndex * Math.ceil(testData.length / NUM_VUS);
	const idx         = sliceStart + exec.vu.iterationInInstance;

	// Guarda-rail: não acessa além dos limites do dataset
	if (idx >= testData.length) return;

	const entry            = testData[idx];
	const expectedApproved = entry.expected_approved;

	const res = http.post(
		'http://localhost:9999/fraud-score',
		JSON.stringify(entry.request),
		{ headers: { 'Content-Type': 'application/json' }, timeout: '2001ms' },
	);

	if (res.status === 200) {
		const body = JSON.parse(res.body);
		if (expectedApproved === body.approved) {
			if (body.approved) tnCount.add(1);
			else               tpCount.add(1);
		} else {
			// Log individual de classificação errada para debug
			const req = entry.request;
			const msg = `ID=${req.id} expected=${expectedApproved} actual=${body.approved} score=${body.fraud_score} amount=${req.transaction.amount} mcc=${req.merchant.mcc}`;
			if (body.approved) {
				fnCount.add(1); // fraude aprovada
				console.log(`FN ${msg}`);
			} else {
				fpCount.add(1); // legítima negada
				console.log(`FP ${msg}`);
			}
		}
	} else {
		errorCount.add(1);
		console.log(`ERROR at idx=${idx}: status=${res.status}`);
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

    const reqWaiting = data.metrics.http_req_waiting ? data.metrics.http_req_waiting.values : {};
    const reqConnecting = data.metrics.http_req_connecting ? data.metrics.http_req_connecting.values : {};
    const reqTlsHandshaking = data.metrics.http_req_tls_handshaking ? data.metrics.http_req_tls_handshaking.values : {};
    const fpScore = data.metrics.fp_score ? data.metrics.fp_score.values : {};
    const fnScore = data.metrics.fn_score ? data.metrics.fn_score.values : {};

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
        scenario: 'precision_probe',
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
            },
            fp_score: {
                min: r(fpScore.min || 0, PRECISION),
                max: r(fpScore.max || 0, PRECISION),
                avg: r(fpScore.avg || 0, PRECISION),
            },
            fn_score: {
                min: r(fnScore.min || 0, PRECISION),
                max: r(fnScore.max || 0, PRECISION),
                avg: r(fnScore.avg || 0, PRECISION),
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
        'test_results/precision.json': JSON.stringify(result, null, 2),
        stdout: textSummary(data, { indent: ' ', enableColors: true }),
    };
}
