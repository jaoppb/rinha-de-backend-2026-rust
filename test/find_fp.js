const fs = require('fs');

async function run() {
    const data = JSON.parse(fs.readFileSync('./test-data.json', 'utf8'));
    for (const entry of data.entries) {
        if (entry.expected_approved === true) {
            const res = await fetch('http://localhost:9999/fraud-score', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(entry.request)
            });
            const body = await res.json();
            if (body.approved === false) {
                console.log("FOUND FP!");
                console.log(JSON.stringify(entry.request, null, 2));
                break;
            }
        }
    }
}
run();
