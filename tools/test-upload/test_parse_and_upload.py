#!/usr/bin/env python3
"""
单元测试：测试日志解析功能
"""

import json
import os
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from threading import Thread
from unittest.mock import patch

from parse_and_upload import GoogleTestParser, GenericParser, upload_test_results


class TestGoogleTestParser(unittest.TestCase):
    """Google Test解析器测试"""
    
    def setUp(self):
        self.parser = GoogleTestParser()
    
    def test_simple_pass(self):
        """测试简单的通过用例"""
        log = """[ RUN      ] TestName
[       OK ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'passed')
        self.assertEqual(result[0]['duration_ms'], 4)
    
    def test_simple_fail(self):
        """测试简单的失败用例（场景2）"""
        log = """[ RUN      ] TestName
test/syscalls/linux/test.cc:42: Failure
Value of: x
Expected: is equal to 1
  Actual: 2
[  FAILED  ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'failed')
        self.assertEqual(result[0]['duration_ms'], 4)
        self.assertIn('error_log', result[0])
        self.assertIn('test/syscalls/linux/test.cc:42', result[0]['error_log'])
    
    def test_fail_without_failed_marker(self):
        """测试失败场景1：只有RUN没有结果但有错误信息"""
        log = """[ RUN      ] TestName
test/syscalls/linux/test.cc:425: Failure
Value of: futex_wake_op(IsPrivate(), &a, &b, 1, 1, (((1 & 0xf) << 28) | ((0 & 0xf) << 24) | ((1 & 0xfff) << 12) | ((kInitialValue + 1) & 0xfff)))
Expected: is equal to 2
  Actual: 1 (of type int)
[ RUN      ] NextTest"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 2)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'failed')
        self.assertIn('error_log', result[0])
        self.assertIn('test/syscalls/linux/test.cc:425', result[0]['error_log'])
    
    def test_multiple_tests(self):
        """测试多个测试用例"""
        log = """[ RUN      ] Test1
[       OK ] Test1 (4 ms)
[ RUN      ] Test2
[       OK ] Test2 (8 ms)
[ RUN      ] Test3
[  FAILED  ] Test3 (12 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 3)
        self.assertEqual(result[0]['name'], 'Test1')
        self.assertEqual(result[0]['status'], 'passed')
        self.assertEqual(result[1]['name'], 'Test2')
        self.assertEqual(result[1]['status'], 'passed')
        self.assertEqual(result[2]['name'], 'Test3')
        self.assertEqual(result[2]['status'], 'failed')
    
    def test_run_not_at_line_start(self):
        """测试[ RUN ]不在行首的情况"""
        log = """[DEBUG] some debug info
[ RUN      ] TestName
[       OK ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'passed')
    
    def test_failed_not_at_line_start(self):
        """测试[ FAILED ]不在行首的情况"""
        log = """[ RUN      ] TestName
[DEBUG] some debug info
[  FAILED  ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'failed')
    
    def test_complex_test_name(self):
        """测试复杂的测试用例名称（包含斜杠、点等）"""
        log = """[ RUN      ] Waiters/WaitAnyChildTest.Fork/0
[       OK ] Waiters/WaitAnyChildTest.Fork/0 (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'Waiters/WaitAnyChildTest.Fork/0')
        self.assertEqual(result[0]['status'], 'passed')
    
    def test_error_log_extraction(self):
        """测试错误日志提取"""
        log = """[ RUN      ] LseekTest.SeekDataAndSeekHole
test/syscalls/linux/lseek.cc:208: Failure
Value of: lseek(fd.get(), mid, 3)
Expected: is equal to 4
  Actual: 8 (of type long)
[  FAILED  ] LseekTest.SeekDataAndSeekHole (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['status'], 'failed')
        self.assertIn('error_log', result[0])
        error_log = result[0]['error_log']
        self.assertIn('test/syscalls/linux/lseek.cc:208', error_log)
        self.assertIn('Expected: is equal to 4', error_log)
        self.assertIn('Actual: 8', error_log)
    
    def test_futx_example(self):
        """测试用户提供的futex示例"""
        log = """[ RUN      ] SharedPrivate/PrivateAndSharedFutexTest.WakeOpCondSuccess/0
test/syscalls/linux/futex.cc:425: Failure
Value of: futex_wake_op(IsPrivate(), &a, &b, 1, 1, (((1 & 0xf) << 28) | ((0 & 0xf) << 24) | ((1 & 0xfff) << 12) | ((kInitialValue + 1) & 0xfff)))
Expected: is equal to 2
  Actual: 1 (of type int)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'SharedPrivate/PrivateAndSharedFutexTest.WakeOpCondSuccess/0')
        self.assertEqual(result[0]['status'], 'failed')
        self.assertIn('error_log', result[0])
        error_log = result[0]['error_log']
        self.assertIn('test/syscalls/linux/futex.cc:425', error_log)
        self.assertIn('Expected: is equal to 2', error_log)
    
    def test_real_world_example(self):
        """测试真实世界的例子（从serial_opt.txt）"""
        log = """[ RUN      ] Waiters/WaitAnyChildTest.Fork/0
[       OK ] Waiters/WaitAnyChildTest.Fork/0 (4 ms)
[ RUN      ] Waiters/WaitAnyChildTest.Fork/1
[       OK ] Waiters/WaitAnyChildTest.Fork/1 (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 2)
        self.assertEqual(result[0]['name'], 'Waiters/WaitAnyChildTest.Fork/0')
        self.assertEqual(result[0]['status'], 'passed')
        self.assertEqual(result[0]['duration_ms'], 4)
        self.assertEqual(result[1]['name'], 'Waiters/WaitAnyChildTest.Fork/1')
        self.assertEqual(result[1]['status'], 'passed')
        self.assertEqual(result[1]['duration_ms'], 4)


class TestGenericParser(unittest.TestCase):
    """通用解析器测试"""
    
    def setUp(self):
        self.parser = GenericParser()
    
    def test_google_test_format(self):
        """测试Google Test格式"""
        log = """[ RUN      ] TestName
[       OK ] TestName (4 ms)"""
        result = self.parser.parse(log)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['name'], 'TestName')
        self.assertEqual(result[0]['status'], 'passed')


class TestUploadResults(unittest.TestCase):
    def upload(self, status, body, headers=None):
        requests = []

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                data = self.rfile.read(int(self.headers["Content-Length"]))
                requests.append((self.path, self.headers, json.loads(data)))
                self.send_response(status)
                for name, value in (headers or {}).items():
                    self.send_header(name, value)
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                requests.append((self.path, self.headers, None))
                self.send_error(405)

            def log_message(self, *args):
                pass

        server = HTTPServer(("127.0.0.1", 0), Handler)
        self.addCleanup(server.server_close)
        thread = Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01})
        thread.start()
        self.addCleanup(thread.join)
        self.addCleanup(server.shutdown)
        self.requests = requests
        with patch.dict(os.environ, {"no_proxy": "127.0.0.1"}):
            return upload_test_results(
                f"http://127.0.0.1:{server.server_port}/api/v1",
                "test-key",
                "fix/example",
                "abc123",
                [{"name": "例子", "status": "failed"}],
            )

    def test_posts_existing_dashboard_payload(self):
        self.assertEqual(self.upload(201, b'{"code":200}'), {"code": 200})
        [(path, headers, payload)] = self.requests
        self.assertEqual(path, "/api/v1/test-runs")
        self.assertEqual(headers["Authorization"], "Bearer test-key")
        self.assertEqual(headers["Content-Type"], "application/json")
        self.assertEqual(payload, {
            "branch_name": "fix/example",
            "commit_id": "abc123",
            "test_type": "gvisor",
            "status": "failed",
            "test_cases": [{"name": "例子", "status": "failed"}],
        })

    def test_http_error_reports_dashboard_message(self):
        with self.assertRaisesRegex(RuntimeError, "422.*invalid commit"):
            self.upload(422, b'{"message":"invalid commit"}')

    def test_http_error_with_non_object_body_reports_status(self):
        with self.assertRaisesRegex(RuntimeError, "502"):
            self.upload(502, b"null")

    def test_redirect_does_not_send_another_request(self):
        with self.assertRaisesRegex(RuntimeError, "302"):
            self.upload(302, b"", {"Location": "/redirect-target"})
        self.assertEqual([path for path, _, _ in self.requests], ["/api/v1/test-runs"])


if __name__ == '__main__':
    unittest.main()

