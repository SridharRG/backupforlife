# PostgreSQL + pgBackRest in one image.
#
# pgBackRest must live in the same container as PostgreSQL: the server invokes
# `pgbackrest archive-push` from archive_command, and backups read PGDATA directly.
ARG PG_IMAGE=postgres:18-alpine
FROM ${PG_IMAGE}

RUN apk add --no-cache pgbackrest \
 && mkdir -p /etc/pgbackrest /var/lib/pgbackrest /var/log/pgbackrest /var/spool/pgbackrest \
 && chown -R postgres:postgres /etc/pgbackrest /var/lib/pgbackrest /var/log/pgbackrest /var/spool/pgbackrest
