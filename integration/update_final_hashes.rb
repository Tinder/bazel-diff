content = File.read("/tmp/final_hashes_json.json").gsub(/:StringGenerator.java": \"\w+\"/, ':StringGenerator.java": "modifiedhash"')
File.write("/tmp/final_hashes_json.json", content)
