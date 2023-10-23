// This is a test program for sqlite3.
// We take it from rcore-os/arceos, thanks to @rcore-os community.
#include <sqlite3.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>

int callback(void *NotUsed, int argc, char **argv, char **azColName)
{
    NotUsed = NULL;

    for (int i = 0; i < argc; ++i) {
        printf("%s = %s\n", azColName[i], (argv[i] ? argv[i] : "NULL"));
    }

    printf("\n");

    return 0;
}

void exec(sqlite3 *db, char *sql)
{
    printf("sqlite exec:\n    %s\n", sql);
    char *errmsg = NULL;
    int rc = sqlite3_exec(db, sql, NULL, NULL, &errmsg);
    if (rc != SQLITE_OK) {
        printf("sqlite exec error: %s\n", errmsg);
    }
}

void query(sqlite3 *db, char *sql)
{
    printf("sqlite query:\n    %s\n", sql);
    char *errmsg = NULL;
    int rc = sqlite3_exec(db, sql, callback, NULL, &errmsg);

    if (rc != SQLITE_OK) {
        printf("%s\n", errmsg);
    }
}

void query_test(sqlite3 *db, const char *args)
{
    puts("======== init user table ========");
    exec(db, "create table user("
             "id INTEGER PRIMARY KEY AUTOINCREMENT,"
             "username TEXT,"
             "password TEXT"
             ")");

    puts("======== insert user 1, 2, 3 into user table ========");

    char cmd[256] = {0};
    sprintf(cmd,
            "insert into user (username, password) VALUES ('%s_1', 'password1'), ('%s_2', "
            "'password2'), ('%s_3', 'password3')",
            args, args, args);
    exec(db, cmd);

    puts("======== select all ========");
    query(db, "select * from user");

    puts("======== select id = 2 ========");
    query(db, "select * from user where id = 2");
}

void memory()
{
    sqlite3 *db;
    printf("sqlite open memory\n");
    int ret = sqlite3_open(":memory:", &db);
    printf("sqlite open memory status %d \n", ret);

    query_test(db, "memory");
}

void file()
{
    sqlite3 *db;
    int ret = sqlite3_open("file.sqlite", &db);
    printf("sqlite open /file.sqlite status %d \n", ret);

    if (ret != 0) {
        printf("sqlite open error");
        return;
    }

    query_test(db, "file");
    sqlite3_close(db);
}

int main()
{
    printf("sqlite version: %s\n", sqlite3_libversion());

    memory();
    file();
    return 0;
}
