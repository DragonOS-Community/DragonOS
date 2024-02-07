# pip install gitpython
import argparse
from git import Repo
import os
import json

parser = argparse.ArgumentParser(
    description='List contributors of DragonOS project')
parser.add_argument('--since', type=str, help='Since date')
parser.add_argument('--until', type=str, help='Until date')
parser.add_argument('--mode', type=str, help='脚本的运行模式 可选：<all> 输出所有信息\n' +
                    ' <short> 输出贡献者名单、邮箱以及提交数量', default='all')
args = parser.parse_args()

repo = Repo(os.path.dirname(os.path.realpath(__file__)) + "/..")

# Get the list of contributors

format = '--pretty={"commit":"%h", "author":"%an", "email":"%ae", "date":"%cd"}'

logs = repo.git.log(format, since=args.since, until=args.until)


if args.mode == 'all':
    print(logs)
elif args.mode == 'short':
    logs = logs.splitlines()
    print("指定时间范围内总共有", len(logs), "次提交")
    logs = [json.loads(line) for line in logs]
    print("贡献者名单：")

    authors = dict()
    for line in logs:
        if line['email'] not in authors.keys():
            authors[line['email']] = {
                'author': line['author'],
                'email': line['email'],
                'count': 1
            }
        else:
            authors[line['email']]['count'] += 1

    # 排序输出
    authors = sorted(authors.values(), key=lambda x: x['count'], reverse=True)
    for author in authors:
        print(author['author'], author['email'], author['count'])
