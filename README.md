This is modified for MG4 to send data to aa-proxy-rs (https://github.com/aa-proxy/aa-proxy-rs)


# 🚗 canze-rs

## Description
This small linux tool is intended to connect to MG4's CAN bus and share the data to aa-proxy-rs<br>

#### This tool gets the following via OBD:
- SOC (state of charge)

It is intended to running constantly (as a daemon) sensing when the car's OBD dongle is in range.

## Usage
```
canze-rs

USAGE:
    canze-rs [OPTIONS]

OPTIONS:
    -c, --config <CONFIG>    Config file path [default: /etc/canze-rs.conf]
    -d, --debug              Enable debug info
    -h, --help               Print help information
    -V, --version            Print version information
```

## Config
The project uses a simple configuration file:<br>
`/etc/canze-rs.conf`<br>

A sample file may have the following contents:<br>
```
[general]
mac = 00:00:00:00:00:00  #enter your bluetooth dongle MAC here

