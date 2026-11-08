"""Download a SWE-bench split (test) as JSON lines from the Hugging Face datasets server.

    fetch.py OUT.jsonl [DATASET]     DATASET: princeton-nlp/SWE-bench_Lite (default) or princeton-nlp/SWE-bench_Verified
"""
import json, sys, urllib.request

DATASET = sys.argv[2] if len(sys.argv) > 2 else "princeton-nlp/SWE-bench_Lite"
URL = "https://datasets-server.huggingface.co/rows?dataset=" + DATASET + "&config=default&split=test&offset={}&length=100"
out = open(sys.argv[1], "w")
n = 0
for offset in range(0, 3000, 100):
    page = json.load(urllib.request.urlopen(URL.format(offset)))
    for row in page["rows"]:
        out.write(json.dumps(row["row"]) + "\n")
        n += 1
    if len(page["rows"]) < 100:
        break
print(n, "tasks")
