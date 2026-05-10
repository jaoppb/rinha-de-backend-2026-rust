import json
import gzip
import struct
import os
import sys

# Default paths
input_file = sys.argv[1] if len(sys.argv) > 1 else 'resources/references.json.gz'
output_file = 'dataset.bin'

print(f'Converting {input_file}...')

if not os.path.exists(input_file):
    print(f"Error: {input_file} not found.")
    sys.exit(1)

# Helper to open either gzip or normal file
def open_input(path):
    if path.endswith('.gz'):
        return gzip.open(path, 'rt')
    return open(path, 'r')

with open_input(input_file) as f_in, open(output_file, 'wb') as f_out:
    data = json.load(f_in)
    for entry in data:
        # 14 floats (56 bytes)
        f_out.write(struct.pack('<14f', *entry['vector']))
        # 1 byte label (0 for legit, 1 for fraud)
        label = 0 if entry['label'] == 'legit' else 1
        f_out.write(struct.pack('B', label))
        # 7 bytes padding
        f_out.write(struct.pack('7x'))

size = os.path.getsize(output_file)
print(f'Done. Created {output_file} ({size} bytes).')
if size % 64 != 0:
    print('Error: Dataset size is not a multiple of 64!')
    sys.exit(1)

# --- MCC Risk Conversion ---
mcc_input = 'resources/mcc_risk.json'
mcc_output = 'mcc_risk.bin'
print(f'Converting {mcc_input}...')
with open(mcc_input, 'r') as f_in, open(mcc_output, 'wb') as f_out:
    mcc_data = json.load(f_in)
    # Write as (u16, f32) pairs
    for mcc, risk in mcc_data.items():
        f_out.write(struct.pack('<Hf', int(mcc), float(risk)))
print(f'Done. Created {mcc_output}.')

# --- Normalization Constants Conversion ---
norm_input = 'resources/normalization.json'
norm_output = 'normalization.bin'
print(f'Converting {norm_input}...')
with open(norm_input, 'r') as f_in, open(norm_output, 'wb') as f_out:
    norm_data = json.load(f_in)
    # Fixed order: max_amount, max_installments, amount_vs_avg_ratio, max_minutes, max_km, max_tx_count_24h, max_merchant_avg_amount
    constants = [
        norm_data['max_amount'],
        norm_data['max_installments'],
        norm_data['amount_vs_avg_ratio'],
        norm_data['max_minutes'],
        norm_data['max_km'],
        norm_data['max_tx_count_24h'],
        norm_data['max_merchant_avg_amount']
    ]
    f_out.write(struct.pack('<7f', *[float(c) for c in constants]))
print(f'Done. Created {norm_output}.')
