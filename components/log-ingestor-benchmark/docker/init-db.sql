-- Creates the `compression_jobs` table that the log-ingestor connector foreign-key-references but
-- does not create itself (it is normally owned by clp-py-utils). This must exist before the mock
-- ingestor connects, so it is mounted into the database container's init directory to run on first
-- initialization. Schema copied from
-- components/clp-py-utils/clp_py_utils/initialize-orchestration-db.py.
CREATE TABLE IF NOT EXISTS `compression_jobs` (
    `id` INT NOT NULL AUTO_INCREMENT,
    `status` INT NOT NULL DEFAULT '0',
    `status_msg` VARCHAR(512) NOT NULL DEFAULT '',
    `creation_time` DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    `start_time` DATETIME(3) NULL DEFAULT NULL,
    `update_time` DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP(),
    `duration` FLOAT NULL DEFAULT NULL,
    `original_size` BIGINT NOT NULL DEFAULT '0',
    `uncompressed_size` BIGINT NOT NULL DEFAULT '0',
    `compressed_size` BIGINT NOT NULL DEFAULT '0',
    `num_tasks` INT NOT NULL DEFAULT '0',
    `num_tasks_completed` INT NOT NULL DEFAULT '0',
    `clp_binary_version` INT NULL DEFAULT NULL,
    `clp_config` MEDIUMBLOB NOT NULL,
    PRIMARY KEY (`id`) USING BTREE,
    INDEX `JOB_STATUS` (`status`) USING BTREE
) ROW_FORMAT=DYNAMIC;
