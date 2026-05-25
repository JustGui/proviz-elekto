#!/usr/bin/env bash
# Populate group "analysis":
#   - Mistral text models (all except open-mistral-nemo)
#   - Groq Llama text models
#   - Groq GPT-OSS text models
set -euo pipefail

PROVIZ="cargo run --bin proviz --"
DB="--db-path ./dev.db"

# 1. Create the group
$PROVIZ $DB group add \
  --slug analysis \
  --name "Analysis" \
  --description "Mistral text + Groq Llama + Groq GPT-OSS models for analysis workloads"

# 2. Mistral text models (excluding open-mistral-nemo), priority by quality desc
$PROVIZ $DB group member add --group analysis --model mistral-large-2512       --priority 10
$PROVIZ $DB group member add --group analysis --model mistral-medium-3-5       --priority 20
$PROVIZ $DB group member add --group analysis --model mistral-medium-2508      --priority 30
$PROVIZ $DB group member add --group analysis --model mistral-medium-2505      --priority 40
$PROVIZ $DB group member add --group analysis --model ministral-14b-2512       --priority 50
$PROVIZ $DB group member add --group analysis --model ministral-8b-2512        --priority 60
$PROVIZ $DB group member add --group analysis --model mistral-small-2603       --priority 70
$PROVIZ $DB group member add --group analysis --model magistral-medium-2509    --priority 80
$PROVIZ $DB group member add --group analysis --model mistral-vibe-cli-latest  --priority 90
$PROVIZ $DB group member add --group analysis --model ministral-3b-2512        --priority 100

# 3. Groq Llama text models
$PROVIZ $DB group member add --group analysis --model llama-3.3-70b-versatile                     --priority 110
$PROVIZ $DB group member add --group analysis --model meta-llama/llama-4-scout-17b-16e-instruct   --priority 120
$PROVIZ $DB group member add --group analysis --model llama-3.1-8b-instant                        --priority 130

# 4. Groq GPT-OSS text models
$PROVIZ $DB group member add --group analysis --model openai/gpt-oss-120b --priority 140
$PROVIZ $DB group member add --group analysis --model openai/gpt-oss-20b  --priority 150

echo "Done — group 'analysis' populated with $(echo '10 + 3 + 2' | bc) models"
