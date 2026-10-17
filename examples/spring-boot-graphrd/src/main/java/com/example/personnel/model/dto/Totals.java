package com.example.personnel.model.dto;

/** The whole organization in one row. */
public record Totals(long employees, long companies, long payroll, double averageSalary,
                     long reviews) {
}
