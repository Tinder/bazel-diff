# PowerShell version of bazel-diff-example.sh for Windows compatibility

param(
    [Parameter(Mandatory=$true)]
    [string]$WorkspacePath,

    [Parameter(Mandatory=$true)]
    [string]$BazelPath,

    [Parameter(Mandatory=$true)]
    [string]$PreviousRevision,

    [Parameter(Mandatory=$true)]
    [string]$FinalRevision
)

$ErrorActionPreference = "Stop"

# Use temp directory for intermediate files
$TempDir = $env:TEMP
$StartingHashesJson = Join-Path $TempDir "starting_hashes.json"
$FinalHashesJson = Join-Path $TempDir "final_hashes.json"
$ImpactedTargetsPath = Join-Path $TempDir "impacted_targets.txt"
$BazelDiff = Join-Path $TempDir "bazel_diff.exe"

# Set appropriate flags based on environment variables
$BazelDiffFlags = @()
if ($env:BAZEL_DIFF_DISABLE_WORKSPACE -eq "true") {
    Write-Host "Disabling workspace for bazel-diff commands (BAZEL_DIFF_DISABLE_WORKSPACE=true)"
    $BazelDiffFlags += "-co"
    $BazelDiffFlags += "--enable_workspace=false"
}

if ($env:BAZEL_DIFF_EXTRA_FLAGS) {
    Write-Host "Injecting extra bazel-diff flags: $env:BAZEL_DIFF_EXTRA_FLAGS"
    $BazelDiffFlags += $env:BAZEL_DIFF_EXTRA_FLAGS.Split(" ")
}

# Bazel command options applied to both top-level bazel invocations and generate-hashes (via -co)
if ($env:BAZEL_EXTRA_COMMAND_OPTIONS) {
    Write-Host "Applying extra Bazel command options to top-level and generate-hashes: $env:BAZEL_EXTRA_COMMAND_OPTIONS"
    $BazelDiffFlags += "-co"
    $BazelDiffFlags += $env:BAZEL_EXTRA_COMMAND_OPTIONS.Split(" ")
}

# Set git checkout flags based on environment variable
$GitCheckoutFlags = @("--quiet")
if ($env:BAZEL_DIFF_FORCE_CHECKOUT -eq "true") {
    Write-Host "Force checkout enabled (BAZEL_DIFF_FORCE_CHECKOUT=true) - will discard uncommitted changes"
    $GitCheckoutFlags = @("--force", "--quiet")
}

# Build bazel-diff and extract script
Write-Host "Building bazel-diff..."
$BazelExtraOptions = @()
if ($env:BAZEL_EXTRA_COMMAND_OPTIONS) {
    $BazelExtraOptions = $env:BAZEL_EXTRA_COMMAND_OPTIONS.Split(" ")
}

& $BazelPath run @BazelExtraOptions :bazel-diff --script_path="$BazelDiff"
if ($LASTEXITCODE -ne 0) {
    throw "Failed to build bazel-diff"
}

# Checkout previous revision
Write-Host "Checking out revision '$PreviousRevision'"
& git -C $WorkspacePath checkout @GitCheckoutFlags $PreviousRevision
if ($LASTEXITCODE -ne 0) {
    throw "Failed to checkout revision $PreviousRevision"
}

# Generate hashes for previous revision
Write-Host "Generating Hashes for Revision '$PreviousRevision'"
& $BazelDiff generate-hashes -w $WorkspacePath -b $BazelPath @BazelDiffFlags $StartingHashesJson
if ($LASTEXITCODE -ne 0) {
    throw "Failed to generate hashes for revision $PreviousRevision"
}

# Checkout final revision
Write-Host "Checking out revision '$FinalRevision'"
& git -C $WorkspacePath checkout @GitCheckoutFlags $FinalRevision
if ($LASTEXITCODE -ne 0) {
    throw "Failed to checkout revision $FinalRevision"
}

# Generate hashes for final revision
Write-Host "Generating Hashes for Revision '$FinalRevision'"
& $BazelDiff generate-hashes -w $WorkspacePath -b $BazelPath @BazelDiffFlags $FinalHashesJson
if ($LASTEXITCODE -ne 0) {
    throw "Failed to generate hashes for revision $FinalRevision"
}

# Determine impacted targets
Write-Host "Determining Impacted Targets"
& $BazelDiff get-impacted-targets -sh $StartingHashesJson -fh $FinalHashesJson -o $ImpactedTargetsPath
if ($LASTEXITCODE -ne 0) {
    throw "Failed to get impacted targets"
}

# Read and display impacted targets
if (Test-Path $ImpactedTargetsPath) {
    $ImpactedTargets = Get-Content $ImpactedTargetsPath
    Write-Host "Impacted Targets between ${PreviousRevision} and ${FinalRevision}:"
    $ImpactedTargets | ForEach-Object { Write-Host $_ }
    Write-Host ""
} else {
    Write-Host "No impacted targets file found"
}
