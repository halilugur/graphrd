package com.example.personnel.model.dto;

/** A company row of the payroll report, computed by the server (a @Query answer). */
public record PayrollRow(String company, long heads, long total, double average,
                         long lowest, long highest) {
}
