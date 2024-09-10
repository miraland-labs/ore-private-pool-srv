clear
# Path to .env
$envFilePath = "D:\ore-private-pool-rewritten\target\release\.env"

# Check if the file exists
if (Test-Path $envFilePath) {
    # Read the file line by line
    Get-Content $envFilePath | ForEach-Object {
        # Ignore comment lines (starting with '#') and empty lines
        if ($_ -match '^\s*$' -or $_ -match '^\s*#') {
            return
        }
        # Split each line into key and value
        $parts = $_ -split '=', 2
        $key = $parts[0].Trim()
        $value = $parts[1].Trim().Trim('"')  # Remove any quotes
        
        # Set the environment variable
        [System.Environment]::SetEnvironmentVariable($key, $value)
    }

    Write-Host -ForegroundColor Green "`n`n`t`t`tThe .env file was successfully loaded and environment variables were set."`n`n`n`n
} else {
    Write-Host -ForegroundColor Red "The .env file was not found: $envFilePath"`n`n`n`n
    $host.Exit()
}

# Test and show environment variables
Write-Host "WALLET:`t`t$([System.Environment]::GetEnvironmentVariable('WALLET_PATH'))"`n
Write-Host "RPC:`t`t$([System.Environment]::GetEnvironmentVariable('RPC_URL'))"`n
Write-Host "RPC WSS:`t$([System.Environment]::GetEnvironmentVariable('RPC_WS_URL'))"`n
Write-Host "SLACK:`t`t$([System.Environment]::GetEnvironmentVariable('SLACK_WEBHOOK'))"`n
Write-Host "DISCORD:`t$([System.Environment]::GetEnvironmentVariable('DISCORD_WEBHOOK'))"`n`n`n

$null = Read-Host 'To continue press ENTER'
cls
# Path to ore-ppl-srv
$SRV = "$HOME/miner/ore-private-pool-srv/target/release/ore-ppl-srv"
Path to wallet
$MKP="$HOME/.config/solana/id.json"

# Default dynamic fee URL. Uncomment next line if you plan to enable dynamic-fee mode
$DYNAMIC_FEE_URL = "YOUR_RPC_URL_HERE"

$BUFFER_TIME = 6
$RISK_TIME = 2
$PRIORITY_FEE = 1000
$PRIORITY_FEE_CAP = 9000

$EXP_MIN_DIFF = 15
$SLACK_DIFF = 23
$XTR_FEE_DIFF = 29
$XTR_FEE_PCT = 50

# Choose transfer mode
$mode = Read-Host "Ore Private Server`nPriority-fee mode 1`nDynamic-fee mode 2`nSelect mode:"

switch ($mode) {
    1 {
        # Priority-fee mode
        $CMD = "$SRV --buffer-time $BUFFER_TIME --risk-time $RISK_TIME --priority-fee $PRIORITY_FEE --priority-fee-cap $PRIORITY_FEE_CAP --expected-min-difficulty $EXP_MIN_DIFF --slack-difficulty $SLACK_DIFF --extra-fee-difficulty $XTR_FEE_DIFF --extra-fee-percent $XTR_FEE_PCT"
    }
    2 {
        # Dynamic-fee mode
        if ($DYNAMIC_FEE_URL -ne "YOUR_RPC_URL_HERE" -and -not [string]::IsNullOrEmpty($DYNAMIC_FEE_URL)) {
            $CMD = "$SRV --buffer-time $BUFFER_TIME --dynamic-fee --dynamic-fee-url $DYNAMIC_FEE_URL --priority-fee-cap $PRIORITY_FEE_CAP --expected-min-difficulty $EXP_MIN_DIFF --slack-difficulty $SLACK_DIFF --extra-fee-difficulty $XTR_FEE_DIFF --extra-fee-percent $XTR_FEE_PCT"
        } else {
            Write-Host "Dynamic Fee URL is not set or is using placeholder value."
            exit
        }
    }
    default {
        Write-Host "Invalid selection. Please enter 1 or 2."
        exit
    }
}

# Display the command
Write-Host $CMD

# Loop to repeat the command if it fails
while ($true) {
    try {
        Invoke-Expression $CMD
        break # Exit the loop if successful
    } catch {
        Write-Host "Starting server command failed. Restarting..."
        Start-Sleep -Seconds 2
    }
}
