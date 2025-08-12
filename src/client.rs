use itertools::Itertools;
use miette::{Context, IntoDiagnostic, Result, bail};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread;
use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;

#[derive(Debug)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub status: ExitStatus,
}

pub struct ScoopClient {
    powershell: PowerShellClient,
    script_path: String,
}

impl ScoopClient {
    /// 新しい Scoop クライアントを作成します。
    pub fn new() -> Result<Self> {
        let powershell = PowerShellClient::new()?;

        let home_dir = String::from_utf8_lossy(
            Command::new("cmd.exe")
                .arg("/c")
                .arg("echo %USERPROFILE%")
                .output()
                .into_diagnostic()
                .wrap_err("failed to get USERPROFILE environment variable")?
                .stdout
                .as_slice(),
        )
        .trim()
        .to_string();
        let script_path = format!(r#"{home_dir}\scoop\apps\scoop\current\bin\scoop.ps1"#);

        Ok(Self {
            powershell,
            script_path,
        })
    }

    pub fn exec(&mut self, commands: &[&str]) -> Result<ExecResult> {
        let mut full_command = vec!["&", self.script_path.as_str()];
        full_command.extend_from_slice(commands);

        self.powershell.exec(&full_command)
    }
}

/// PowerShell プロセスを保持し、コマンド実行を仲介するクライアント。
// scoop コマンドは実行ごとに毎回 PowerShell バイナリを起動するため、そのオーバーヘッドが非常に大き
// い。また、おそらく一度ロードした .ps1 ファイルのキャッシュを持っているようで、スクリプト直接実行
// の場合は二回目以降の実行が特に顕著に早くなる。そのため、scoop depends ... を多数実行するようなユ
// ースケースでは同じ PowerShell インスタンスをなるべく使い回すようにしたい。
pub struct PowerShellClient {
    process: Child,
    stdin: ChildStdin,
}

impl PowerShellClient {
    /// 新しい PowerShell クライアントを作成し、バックグラウンドで PowerShell プロセスを起動します。
    pub fn new() -> Result<Self> {
        let mut process = Command::new("pwsh.exe")
            .args([
                "-NoLogo",         // ロゴを表示しない
                "-NoProfile",      // プロファイルスクリプトを読み込まない
                "-NonInteractive", // 対話モードにしない
                "-Command",        // 標準入力からコマンドを受け取る
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .into_diagnostic()
            .wrap_err("failed to spawn powershell process")?;

        let stdin = process.stdin.take().expect("Failed to open stdin");
        let mut client = Self { process, stdin };

        // PowerShell の出力エンコーディングをUTF-8に設定
        client
            .stdin
            .write_all(b"$OutputEncoding = [System.Text.Encoding]::UTF8\n")
            .into_diagnostic()
            .wrap_err("failed to configure output encoding to UTF-8")?;

        Ok(client)
    }

    /// 起動中のPowerShellプロセス上でコマンドを実行します。
    pub fn exec(&mut self, command_and_args: &[&str]) -> Result<ExecResult> {
        // 実行するコマンドをスペースで連結
        let quote = |s: &&str| {
            if s.contains('"') {
                // ダブルクォートが含まれている場合はシングルクォートで囲む
                format!("'{s}'")
            } else if s.contains(' ') || s.contains('\'') {
                // シングルクォートが含まれている場合はダブルクォートで囲む
                format!("\"{s}\"")
            } else {
                // 特に問題がなければそのまま
                s.to_string()
            }
        };
        let command_str = command_and_args.iter().map(quote).join(" ");

        // 行単位で出力をパースしていくが、そのままだとコマンドが終了したのかただ出力がないまま時間
        // がかかっているのかを判別できないため、コマンド終了時にこのマーカーを出力するようにする。
        // 当然ここで指定したマーカーがコマンドの出力に含まれてしまうと誤作動はする...
        const END_MARKER: &str = "----------END_OF_COMMAND----------";

        // 実際のコマンド、終了コードの取得、マーカーの出力を一連のコマンドとして組み立てる
        let full_command = [
            &*format!("{command_str}\n"),
            "$exitCode = $LASTEXITCODE\n",
            "[Console]::Out.WriteLine(\"EXIT_CODE:$exitCode\")\n",
            &*format!("[Console]::Out.WriteLine('{END_MARKER}')\n"),
            &*format!("[Console]::Error.WriteLine('{END_MARKER}')\n"),
        ]
        .join("");
        self.stdin
            .write_all(full_command.as_bytes())
            .into_diagnostic()
            .wrap_err("failed to write the command")?;
        self.stdin
            .flush()
            .into_diagnostic()
            .wrap_err("failed to send the command")?;

        // 別スレッドで読み込む。そうでないとバッファを越えた出力があったときにデッドロックしてしま
        // う。
        fn make_handler<R: Read>(reader: R) -> impl FnOnce() -> (R, String) {
            move || {
                let mut output = String::new();
                let mut line = String::new();
                let mut reader = BufReader::new(reader);
                while let Ok(len) = reader.read_line(&mut line) {
                    if len == 0 || line.trim() == END_MARKER {
                        break;
                    }
                    output.push_str(&line);
                    line.clear();
                }
                (reader.into_inner(), output)
            }
        }

        let stdout = self.process.stdout.take().expect("Failed to take stdout");
        let stdout_thread = thread::spawn(make_handler(stdout));

        let stderr = self.process.stderr.take().expect("Failed to take stderr");
        let stderr_thread = thread::spawn(make_handler(stderr));

        let (stdout, mut stdout_str) = stdout_thread.join().unwrap();
        let (stderr, stderr_str) = stderr_thread.join().unwrap();
        self.process.stdout = Some(stdout);
        self.process.stderr = Some(stderr);

        // 出力から EXIT_CODE をパース
        let Some(pos) = stdout_str.rfind("EXIT_CODE:") else {
            bail!("failed to find EXIT_CODE in stdout: {stdout_str}");
        };
        let code_str = &stdout_str[pos + "EXIT_CODE:".len()..].trim();
        let exit_code = code_str.parse::<i32>().unwrap_or(-1);
        stdout_str.truncate(pos); // EXIT_CODEの部分を削除
        let status = ExitStatus::from_raw(exit_code);

        Ok(ExecResult {
            stdout: stdout_str.trim_end().to_string(),
            stderr: stderr_str.trim_end().to_string(),
            status,
        })
    }
}

/// クライアントが破棄されるときにPowerShellプロセスを終了させます。
impl Drop for PowerShellClient {
    fn drop(&mut self) {
        // "exit" コマンドを送信してプロセスをクリーンに終了
        let _ = self.stdin.write_all(b"exit\n");
        let _ = self.stdin.flush();
        // プロセスの終了を待つ
        let _ = self.process.wait();
        println!("\nPowerShell process terminated.");
    }
}
