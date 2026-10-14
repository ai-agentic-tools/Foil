"""Download SWE-bench Lite (test split) as JSON lines from the Hugging Face datasets server."""
import json, sys, urllib.request

URL = "https://datasets-server.huggingface.co/rows?dataset=princeton-nlp/SWE-bench_Lite&config=default&split=test&offset={}&length=100"
out = open(sys.argv[1], "w")
n = 0
for offset in range(0, 400, 100):
    page = json.load(urllib.request.urlopen(URL.format(offset)))
    for row in page["rows"]:
        out.write(json.dumps(row["row"]) + "\n")
        n += 1
    if len(page["rows"]) < 100:
        break
print(n, "tasks")
