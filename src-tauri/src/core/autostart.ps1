$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
try {
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $name = 'BongoCat-Autostart-' + $sid
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $folder = $service.GetFolder('\')
    $existing = @($folder.GetTasks(0) | Where-Object Name -eq $name)
    if ($env:BONGO_AUTOSTART_ENABLED -eq '1') {
        $exe = $env:BONGO_AUTOSTART_EXE
        if (!(Test-Path -LiteralPath $exe -PathType Leaf)) { throw 'Application executable not found' }
        $task = $service.NewTask(0)
        $task.RegistrationInfo.Description = 'BongoCat: start for this user at sign-in'
        $task.Principal.UserId = $sid
        $task.Principal.LogonType = 3 # InteractiveToken: never run on the login screen
        $task.Principal.RunLevel = 1 # HighestAvailable; registration requires elevation
        $task.Settings.Enabled = $true
        $task.Settings.StartWhenAvailable = $true
        $task.Settings.DisallowStartIfOnBatteries = $false
        $task.Settings.StopIfGoingOnBatteries = $false
        $task.Settings.ExecutionTimeLimit = 'PT0S'
        $task.Settings.MultipleInstances = 2 # IgnoreNew
        $trigger = $task.Triggers.Create(9) # Logon
        $trigger.UserId = $sid
        $trigger.Enabled = $true
        $action = $task.Actions.Create(0)
        $action.Path = $exe
        $action.WorkingDirectory = [IO.Path]::GetDirectoryName($exe)
        $null = $folder.RegisterTaskDefinition($name, $task, 6, $sid, $null, 3)
        $registered = $folder.GetTask($name)
        if (!$registered.Enabled -or $registered.Definition.Actions.Item(1).Path -ne $exe) {
            throw 'Startup task verification failed'
        }
    } elseif ($existing.Count -gt 0) {
        $folder.DeleteTask($name, 0)
    }
    # Migrate only after successful registration/deletion. Keep the old entry on failure.
    $run = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Run', $true)
    if ($null -ne $run) {
        try { $run.DeleteValue('BongoCat', $false) } finally { $run.Dispose() }
    }
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
