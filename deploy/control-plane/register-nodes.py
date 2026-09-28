#!/usr/bin/env python3
"""Register control-plane agents through Cloud after the API quorum is ready.

The deployment runner holds a short-lived cloud-admin session. Each machine
receives only its own node credential; neither etcd nor an admin token is
copied to the joining node.
"""
import json
import os
from pathlib import Path
import shlex
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.request


def fail(message):
    raise SystemExit(message)


def main(inventory_path):
    inventory = json.loads(Path(inventory_path).read_text())
    registration = inventory['api']['nodeRegistration']
    for field in ('url', 'ca', 'adminTokenFile', 'agentUrl'):
        if not registration.get(field):
            fail(f'api.nodeRegistration.{field} is required')
    url = registration['url'].rstrip('/')
    agent_url = registration['agentUrl'].rstrip('/')
    if not url.startswith('https://') or not agent_url.startswith('https://'):
        fail('control-plane registration requires HTTPS API and agent URLs')
    token_path = Path(registration['adminTokenFile'])
    if token_path.stat().st_mode & 0o077:
        fail('api.nodeRegistration.adminTokenFile must be readable only by its owner')
    admin_token = token_path.read_text().strip()
    if not admin_token:
        fail('admin token file is empty')
    context = ssl.create_default_context(cafile=registration['ca'])
    ssh = inventory['ssh']
    key = os.environ.get('VELSTRA_DEPLOY_SSH_KEY') or ssh['privateKey']
    user = os.environ.get('VELSTRA_DEPLOY_SSH_USER') or ssh.get('user', 'root')
    opts = ['-F', '/dev/null', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
            '-i', key]
    known = os.environ.get('VELSTRA_DEPLOY_KNOWN_HOSTS')
    if known:
        opts.extend(['-o', 'StrictHostKeyChecking=yes', '-o', f'UserKnownHostsFile={known}'])
    else:
        opts.extend(['-o', 'StrictHostKeyChecking=accept-new'])
    root = '' if user == 'root' else 'sudo '

    def request(method, path, body=None, expected=(200,)):
        data = None if body is None else json.dumps(body).encode()
        headers = {'Authorization': 'Bearer ' + admin_token,
                   'Content-Type': 'application/json'}
        call = urllib.request.Request(url + '/api/v1' + path, data=data,
                                      headers=headers, method=method)
        try:
            with urllib.request.urlopen(call, context=context, timeout=15) as response:
                status, payload = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, payload = error.code, error.read()
        if status not in expected:
            fail(f'{method} {path}: Cloud API returned HTTP {status}')
        return status, json.loads(payload) if payload else {}

    def remote(node, command, *, input_text=None):
        return subprocess.run(['ssh', *opts, f'{user}@{node["sshAddress"]}',
                               root + command], input=input_text, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              check=False)

    for node in inventory['controlPlanes']:
        name = node['name']
        present = remote(node, 'sh -c "if test -s /etc/velstra/node-token || test -s /var/lib/velstra/node-token; then echo present; else echo missing; fi"')
        if present.returncode != 0 or present.stdout.strip() not in ('present', 'missing'):
            fail(f'{name}: cannot inspect local node credential over SSH')
        code, _ = request('GET', '/nodes/' + name, expected=(200, 404))
        if present.stdout.strip() == 'missing':
            if code == 404:
                _, issued = request('POST', '/nodes',
                                    {'id': name, 'spec': {'schedulable': False}},
                                    expected=(202,))
            else:
                _, issued = request('POST', '/nodes/' + name + ':issueCredential',
                                    {}, expected=(200, 201))
            token = issued.get('nodeToken')
            if not token:
                fail(f'{name}: Cloud API did not return a node credential')
            written = remote(node, 'sh -c "umask 077; cat > /etc/velstra/node-token"',
                             input_text=token + '\n')
            if written.returncode:
                fail(f'{name}: could not deliver node credential over SSH')
            configured = remote(node, 'bash -s', input_text='''set -euo pipefail
test -s /etc/velstra/node.env
grep -Eq '^VELSTRA_ROLES=.*control-plane' /etc/velstra/node.env
python3 - '''+shlex.quote(agent_url)+''' <<'PY'
from pathlib import Path
import sys
path=Path('/etc/velstra/node.env')
lines=[line for line in path.read_text().splitlines() if not line.startswith('VELSTRA_API_URL=')]
lines.append('VELSTRA_API_URL='+sys.argv[1])
path.write_text('\\n'.join(lines)+'\\n')
PY
systemctl enable velstra-cloud-nodeagent >/dev/null
systemctl restart velstra-cloud-nodeagent
''')
            if configured.returncode:
                fail(f'{name}: could not start the credentialed node agent')
        elif code == 404:
            fail(f'{name}: agent credential exists but node object is missing; inspect before minting another identity')
        if remote(node, 'systemctl is-active --quiet velstra-cloud-nodeagent').returncode:
            fail(f'{name}: node agent is not running')
        for attempt in range(20):
            _, current = request('GET', '/nodes/' + name)
            heartbeat = current.get('status', {}).get('lastHeartbeat') or 0
            if isinstance(heartbeat, int) and time.time() * 1000 - heartbeat < 30000:
                break
            time.sleep(2)
        else:
            fail(f'{name}: node agent has no fresh heartbeat')
        print(f'{name}: control-plane agent registered and reporting')


if __name__ == '__main__':
    if len(sys.argv) != 2:
        fail('usage: register-nodes.py INVENTORY.json')
    main(sys.argv[1])
