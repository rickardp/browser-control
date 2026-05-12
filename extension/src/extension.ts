import * as vscode from 'vscode';
import * as http from 'http';

const ENV_VAR = 'BROWSER_CONTROL';
const CONFIG_SECTION = 'browserControl';

let statusBar: vscode.StatusBarItem;
let currentEndpoint: string | undefined;

interface VersionInfo {
	Browser?: string;
	webSocketDebuggerUrl?: string;
}

function probePort(port: number, timeoutMs = 200): Promise<VersionInfo | undefined> {
	return new Promise((resolve) => {
		const req = http.get(
			{ host: '127.0.0.1', port, path: '/json/version', timeout: timeoutMs },
			(res) => {
				if (res.statusCode !== 200) {
					res.resume();
					resolve(undefined);
					return;
				}
				const chunks: Buffer[] = [];
				res.on('data', (c: Buffer) => chunks.push(c));
				res.on('end', () => {
					try {
						const parsed = JSON.parse(Buffer.concat(chunks).toString('utf8')) as VersionInfo;
						resolve(parsed);
					} catch {
						resolve(undefined);
					}
				});
			},
		);
		req.on('timeout', () => {
			req.destroy();
			resolve(undefined);
		});
		req.on('error', () => resolve(undefined));
	});
}

async function discoverEndpoint(): Promise<string | undefined> {
	const cfg = vscode.workspace.getConfiguration(CONFIG_SECTION);
	const explicit = (cfg.get<string>('endpoint') ?? '').trim();
	if (explicit.length > 0) {
		return explicit;
	}
	const ports = cfg.get<number[]>('probePorts') ?? [9222, 9223, 9224, 9225, 9226, 9227, 9228, 9229];
	for (const port of ports) {
		const info = await probePort(port);
		if (!info) {
			continue;
		}
		const browser = info.Browser ?? '';
		if (/electron|chrome|chromium/i.test(browser) && info.webSocketDebuggerUrl) {
			return info.webSocketDebuggerUrl;
		}
	}
	return undefined;
}

function shortenEndpoint(url: string): string {
	try {
		const u = new URL(url);
		return `${u.protocol}//${u.hostname}:${u.port}`;
	} catch {
		return url.length > 40 ? url.slice(0, 37) + '...' : url;
	}
}

function updateStatusBar(endpoint: string | undefined): void {
	if (endpoint) {
		statusBar.text = `BC: ${shortenEndpoint(endpoint)}`;
		statusBar.tooltip = `Browser Control CDP endpoint: ${endpoint}\nClick to copy.`;
	} else {
		statusBar.text = 'BROWSER_CONTROL: not discovered';
		statusBar.tooltip = 'No CDP endpoint discovered. Open the Simple Browser, then run "Browser Control: Refresh".';
	}
	statusBar.show();
}

async function refresh(context: vscode.ExtensionContext): Promise<void> {
	const endpoint = await discoverEndpoint();
	currentEndpoint = endpoint;
	if (endpoint) {
		context.environmentVariableCollection.replace(ENV_VAR, endpoint);
		context.environmentVariableCollection.description = 'CDP endpoint of VS Code Simple Browser';
	} else {
		context.environmentVariableCollection.delete(ENV_VAR);
	}
	updateStatusBar(endpoint);
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
	statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
	statusBar.command = 'browser-control.copyEndpoint';
	context.subscriptions.push(statusBar);

	context.subscriptions.push(
		vscode.commands.registerCommand('browser-control.copyEndpoint', async () => {
			if (!currentEndpoint) {
				vscode.window.showWarningMessage('Browser Control: no CDP endpoint discovered yet.');
				return;
			}
			await vscode.env.clipboard.writeText(currentEndpoint);
			vscode.window.showInformationMessage(`Copied CDP endpoint: ${currentEndpoint}`);
		}),
		vscode.commands.registerCommand('browser-control.refresh', async () => {
			await refresh(context);
			if (currentEndpoint) {
				vscode.window.showInformationMessage(`Browser Control: discovered ${currentEndpoint}`);
			} else {
				vscode.window.showWarningMessage('Browser Control: no CDP endpoint found on configured ports.');
			}
		}),
		vscode.workspace.onDidChangeConfiguration((e) => {
			if (e.affectsConfiguration(CONFIG_SECTION)) {
				void refresh(context);
			}
		}),
	);

	await refresh(context);
}

export function deactivate(): void {
	statusBar?.dispose();
}
