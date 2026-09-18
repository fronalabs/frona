// Low-disk equivalent of running all workspace integration test targets.
// Only final test links use LLD and omit debug sections; dependencies and test
// behavior are unchanged. Unit/bin/doc tests are separate cargo commands.
import { spawn } from 'node:child_process';
import { readFile, writeFile } from 'node:fs/promises';

async function run(command, args, capture = false) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: ['ignore', 'pipe', 'pipe'] });
    let output = '';
    child.stdout.on('data', data => { output += data; if (!capture) process.stdout.write(data); });
    child.stderr.on('data', data => { if (!capture) process.stderr.write(data); });
    child.on('error', reject);
    child.on('close', code => resolve({ code, output }));
  });
}
const metadata = await run('cargo', ['metadata', '--no-deps', '--format-version=1'], true);
if (metadata.code !== 0) throw new Error('cargo metadata failed');
const packages = JSON.parse(metadata.output).packages;
const retryFailed = process.argv.includes('--retry-failed');
const results = retryFailed ? JSON.parse(await readFile('target/provider-integration-results.json', 'utf8')) : [];
function record(result) {
  const index = results.findIndex(previous => previous.package === result.package && previous.target === result.target);
  if (index < 0) results.push(result);
  else results[index] = { ...result, previousAttempt: results[index] };
}
for (const pkg of packages) {
  for (const target of pkg.targets.filter(target => target.kind.includes('test'))) {
    if (retryFailed && !results.some(result => result.package === pkg.name && result.target === target.name && result.code !== 0)) continue;
    console.log(`Checking integration target ${pkg.name}/${target.name}`);
    const args = ['rustc', '-j', '1', '-p', pkg.name, '--test', target.name, '--message-format=json', '--', '-C', 'link-arg=-fuse-ld=lld', '-C', 'link-arg=-Wl,--strip-debug'];
    const compiled = await run('cargo', args, true);
    const messages = compiled.output.split('\n').filter(Boolean).map(line => JSON.parse(line));
    const executable = messages.find(message => message.reason === 'compiler-artifact' && message.target.name === target.name && message.executable)?.executable;
    if (compiled.code !== 0 || !executable) {
      for (const message of messages) if (message.reason === 'compiler-message') console.error(message.message.rendered);
      record({ package: pkg.name, target: target.name, phase: 'compile', code: compiled.code ?? 1 });
      continue;
    }
    const tested = await run(executable, []);
    record({ package: pkg.name, target: target.name, phase: 'test', code: tested.code,
      summaries: tested.output.split('\n').filter(line => line.startsWith('test result:')) });
  }
}
await writeFile('target/provider-integration-results.json', JSON.stringify(results, null, 2));
console.log(JSON.stringify({ targets: results.length, failed: results.filter(result => result.code !== 0) }, null, 2));
process.exitCode = results.some(result => result.code !== 0) ? 1 : 0;
