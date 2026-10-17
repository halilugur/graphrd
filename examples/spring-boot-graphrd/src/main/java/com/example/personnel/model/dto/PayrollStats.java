package com.example.personnel.model.dto;

/** The employee-side aggregate of the whole organization (a @Query answer). */
public record PayrollStats(long employees, long payroll, double average) {
}
