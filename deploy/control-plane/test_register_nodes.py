"""Role and agent trust reconciliation during control-plane deployment."""
import importlib.util
from pathlib import Path
import unittest
import os
import subprocess
import tempfile

module = importlib.util.spec_from_file_location(
    'register_nodes', Path(__file__).with_name('register-nodes.py'))
registration = importlib.util.module_from_spec(module)
module.loader.exec_module(registration)


class ControlPlaneRoles(unittest.TestCase):
    def test_legacy_compute_semantics_survive_registration(self):
        for spec in ({}, {'roles': []}):
            with self.subTest(spec=spec):
                self.assertEqual(registration.control_plane_roles(spec),
                                 ['compute', 'control-plane'])

    def test_explicit_services_are_preserved(self):
        spec = {'roles': ['compute', 'storage']}
        self.assertEqual(registration.control_plane_roles(spec),
                         ['compute', 'storage', 'control-plane'])
        self.assertEqual(spec['roles'], ['compute', 'storage'])

    def test_control_only_nodes_stay_control_only_on_repeated_runs(self):
        spec = {'roles': ['control-plane']}
        self.assertEqual(registration.control_plane_roles(spec), ['control-plane'])


class AgentTrust(unittest.TestCase):
    def test_trust_path_changes_even_when_endpoint_stays_the_same(self):
        lines = ['VELSTRA_NODE=node-a', 'VELSTRA_API_URL=https://api.example',
                 'VELSTRA_API_CA=/old/api.pem']
        updated = registration.agent_seed(lines, 'https://api.example', '/new/ca.pem')
        self.assertIn('VELSTRA_NODE=node-a', updated)
        self.assertIn('VELSTRA_API_CA=/new/ca.pem', updated)
        self.assertNotIn('VELSTRA_API_CA=/old/api.pem', updated)
        self.assertEqual(lines[-1], 'VELSTRA_API_CA=/old/api.pem')

    def test_duplicate_endpoints_and_trust_paths_are_removed(self):
        lines = ['VELSTRA_API_URL=https://old.example', 'VELSTRA_API_URL=https://api.example',
                 'VELSTRA_API_CA=/ca.pem', 'VELSTRA_API_CA=/old.pem']
        self.assertEqual(registration.agent_seed(lines, 'https://api.example', '/ca.pem'),
                         ['VELSTRA_API_URL=https://api.example', 'VELSTRA_API_CA=/ca.pem'])

    def test_unchanged_trust_and_endpoint_are_idempotent(self):
        lines = ['# machine identity', 'VELSTRA_NODE=node-a',
                 'VELSTRA_API_URL=https://api.example', 'VELSTRA_API_CA=/ca.pem']
        self.assertEqual(registration.agent_seed(lines, 'https://api.example', '/ca.pem'), lines)


class AgentReload(unittest.TestCase):
    def test_shared_trust_reloads_pool_and_node_agents_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'node.env').write_text('VELSTRA_ROLES=control-plane\nVELSTRA_API_URL=https://old.example\nVELSTRA_API_CA=/old/ca.pem\n')
            ca = root / 'ca.pem'
            ca.write_text('first trust bundle')
            commands = root / 'commands'
            binary = root / 'bin'
            binary.mkdir()
            for name, body in {
                'curl': 'test "${FAIL_READY:-0}" = 0',
                'systemctl': 'echo "$*" >>"$COMMAND_LOG"\nif [ "$1" = try-restart ] && [ "${FAIL_POOL:-0}" = 1 ]; then exit 1; fi',
            }.items():
                file = binary / name
                file.write_text('#!/bin/sh\n' + body + '\n')
                file.chmod(0o755)
            script = registration.endpoint_script('https://api.example', str(ca))
            script = script.replace('/etc/velstra/node.env', str(root / 'node.env')).replace('/run/velstra-agent-', str(root / 'agent-'))
            env = dict(os.environ, PATH=str(binary)+os.pathsep+os.environ['PATH'], COMMAND_LOG=str(commands))
            def run(**extra):
                return subprocess.run(['bash', '-s'], input=script, text=True, env=dict(env, **extra), capture_output=True)
            self.assertNotEqual(run(FAIL_READY='1').returncode, 0)
            self.assertFalse(commands.exists(), 'do not reload agents against an unavailable endpoint')
            first = run()
            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertIn('try-restart velstra-cloud-poolagent*.service', commands.read_text())
            self.assertIn('restart velstra-cloud-nodeagent', commands.read_text())
            commands.write_text('')
            self.assertEqual(run().returncode, 0)
            self.assertNotIn('restart', commands.read_text(), 'unchanged credentials must not disrupt active transfers')
            ca.write_text('rotated trust bundle')
            self.assertNotEqual(run(FAIL_POOL='1').returncode, 0)
            self.assertTrue((root / 'agent-endpoint-changed').exists(), 'keep failed reload pending')
            commands.write_text('')
            self.assertEqual(run().returncode, 0)
            self.assertIn('try-restart velstra-cloud-poolagent*.service', commands.read_text())
            self.assertFalse((root / 'agent-endpoint-changed').exists())


if __name__ == '__main__':
    unittest.main()
