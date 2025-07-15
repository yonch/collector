#!/bin/bash

# Script to plot all off-CPU analysis CSV files
# Usage: ./plot_all_off_cpu.sh [csv_directory]

# Set default directory to current directory if not provided
CSV_DIR=${1:-.}

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "Looking for off-CPU CSV files in: $CSV_DIR"
echo "Using plotting script: $SCRIPT_DIR/off_cpu_analysis.R"

# Check if the R script exists
if [[ ! -f "$SCRIPT_DIR/off_cpu_analysis.R" ]]; then
    echo "Error: off_cpu_analysis.R not found in $SCRIPT_DIR"
    exit 1
fi

# Find and process all off-CPU CSV files
found_files=0

for csv_file in "$CSV_DIR"/*_per_cpu_off_time.csv "$CSV_DIR"/*_global_off_time.csv "$CSV_DIR"/*_per_cpu_other_cpu_time.csv "$CSV_DIR"/*_global_other_cpu_time.csv; do
    if [[ -f "$csv_file" ]]; then
        echo "Processing: $csv_file"
        Rscript "$SCRIPT_DIR/off_cpu_analysis.R" "$csv_file"
        
        if [[ $? -eq 0 ]]; then
            echo "✓ Successfully created plot for $csv_file"
        else
            echo "✗ Failed to create plot for $csv_file"
        fi
        echo ""
        
        ((found_files++))
    fi
done

if [[ $found_files -eq 0 ]]; then
    echo "No off-CPU CSV files found in $CSV_DIR"
    echo "Expected files with patterns:"
    echo "  *_per_cpu_off_time.csv"
    echo "  *_global_off_time.csv" 
    echo "  *_per_cpu_other_cpu_time.csv"
    echo "  *_global_other_cpu_time.csv"
    exit 1
else
    echo "Processed $found_files off-CPU CSV files"
fi